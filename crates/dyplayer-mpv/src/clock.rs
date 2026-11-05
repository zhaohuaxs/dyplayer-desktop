use std::sync::Mutex;
use std::time::Instant;

/// 媒体时钟原子快照（R08）：供跨线程消费者（GPU 覆盖层）自行外推
/// media_time，不依赖主线程每帧推送的 f64——主线程卡顿时覆盖层仍能
/// 按上次锚点 + 速率继续平滑推进。
#[derive(Clone)]
pub struct ClockSnapshot {
    pub time_pos: f64,
    pub wall_at_calib: Instant,
    pub speed: f64,
    pub speed_factor: f64,
    pub paused: bool,
    pub resume_pending: bool,
    pub stalled: bool,
}

impl ClockSnapshot {
    /// 按快照外推当前媒体时间（与 MediaClock::media_time_at 同公式）。
    pub fn media_time_at(&self, now: Instant) -> f64 {
        if self.paused || self.resume_pending || self.stalled {
            return self.time_pos;
        }
        let dt = now
            .saturating_duration_since(self.wall_at_calib)
            .as_secs_f64();
        self.time_pos + dt * self.speed * self.speed_factor
    }
}

impl Default for ClockSnapshot {
    fn default() -> Self {
        Self {
            time_pos: 0.0,
            wall_at_calib: Instant::now(),
            speed: 1.0,
            speed_factor: 1.0,
            paused: true,
            resume_pending: false,
            stalled: false,
        }
    }
}

/// 软校准死区（秒）：偏差在此内不动锚。time-pos 事件链路
/// （mpv 采样 → 事件线程处理）的延迟抖动落在该量级，硬锚定会把
/// 抖动原样打回 media_time，表现为弹幕周期性后撤再重滚。
const SMOOTH_DEAD_ZONE: f64 = 0.050;

/// 软校准硬锚阈值（秒）：偏差超过视为真实跳变（seek 落点钳制、
/// 换集残留事件等），无条件锚定。
const JUMP_THRESHOLD: f64 = 0.250;

/// 死区与阈值之间的收敛比例：每次向事件值收敛一半，消化外推与
/// mpv 时钟的慢速漂移，单次可见位移减半。
const SMOOTH_CONVERGE: f64 = 0.5;

/// 外推速度上限：mpv speed 事件可能携带异常大值（用户调速或解码器
/// 瞬态），钳制防止外推飞超前 time-pos 校准能力。覆盖常见 16x 快进。
const SPEED_MAX: f64 = 16.0;

/// 收敛区降速幅度上限：事件滞后时最多把外推速度压到 50%，用"弹幕
/// 慢一拍"换"弹幕不被拉回"，偏差在后续事件中消化而非瞬时回撤。
const SLOWDOWN_MAX: f64 = 0.5;

struct ClockState {
    time_pos: f64,
    wall_at_calib: Instant,
    speed: f64,
    paused: bool,
    /// 恢复期等首批 time-pos 事件解冻：set_paused(false) 后置 true，
    /// media_time 冻结在暂停点，首批 calibrate_smooth 到达硬锚解冻。
    /// 消除"外推先快走、mpv 起播滞后、校准回拉"的恢复期回撤。
    resume_pending: bool,
    /// 收敛区降速因子：事件滞后时 <1.0 减速消化偏差，正常 1.0。
    /// 由 calibrate_smooth_at 设置，set_speed/calibrate_at/死区重置。
    speed_factor: f64,
    /// 实际播放推进停滞（缓冲/加载/EOF/错误，由 core-idle 驱动，R04）。
    /// 与用户 pause 区分：pause 是用户意图，stalled 是 mpv 实际未推进。
    /// stalled 时 media_time 冻结在最后锚定值，避免无 time-pos 间隙外推漂移。
    stalled: bool,
}

/// 媒体时钟：mpv `time-pos` 事件校准 + `speed` 帧间插值（TDD 6.3）。
///
/// `last_time + Δwallclock × speed` 逐帧插值；事件到达时校准；pause 冻结、
/// seek 由事件立即校准。线程安全：mpv 事件线程写、UI 渲染线程读。
pub struct MediaClock {
    state: Mutex<ClockState>,
}

impl Default for MediaClock {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaClock {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(ClockState {
                time_pos: 0.0,
                wall_at_calib: Instant::now(),
                speed: 1.0,
                paused: true,
                resume_pending: false,
                speed_factor: 1.0,
                stalled: false,
            }),
        }
    }

    pub fn calibrate(&self, time_pos: f64) {
        let now = Instant::now();
        self.with_state(|state| Self::calibrate_at(state, time_pos, now));
    }

    /// time-pos 事件的软校准：事件值携带的是 mpv 采样时刻的媒体时间，
    /// 采样→处理之间的链路延迟抖动会使事件值偏离外推值数十毫秒。
    /// 死区内不动锚（外推是当下时刻的更好估计）；介于死区与
    /// `JUMP_THRESHOLD` 之间向事件值收敛一半（消化慢速漂移）；
    /// 超过则视为真实跳变硬锚定。seek/FileLoaded/Restart 走
    /// [`MediaClock::calibrate`]（无条件锚定）。
    pub fn calibrate_smooth(&self, time_pos: f64) {
        let now = Instant::now();
        self.with_state(|state| Self::calibrate_smooth_at(state, time_pos, now));
    }

    pub fn set_speed(&self, speed: f64) {
        let now = Instant::now();
        self.with_state(|state| Self::set_speed_at(state, speed, now));
    }

    pub fn set_paused(&self, paused: bool) {
        let now = Instant::now();
        self.with_state(|state| Self::set_paused_at(state, paused, now));
    }

    /// 标记实际播放推进是否停滞（core-idle 驱动，R04）。停滞时冻结外推，
    /// 恢复时从当前锚点继续推进。与用户 pause 独立：pause+stalled 任一为真
    /// media_time 都冻结。
    pub fn set_stalled(&self, stalled: bool) {
        let now = Instant::now();
        self.with_state(|state| Self::set_stalled_at(state, stalled, now));
    }

    pub fn now(&self) -> f64 {
        let now = Instant::now();
        self.with_state(|state| Self::media_time_at(state, now))
    }

    /// 产生原子快照（R08）：供覆盖线程自行外推，不依赖主线程每帧推送。
    pub fn snapshot(&self) -> ClockSnapshot {
        self.with_state(|state| ClockSnapshot {
            time_pos: state.time_pos,
            wall_at_calib: state.wall_at_calib,
            speed: state.speed,
            speed_factor: state.speed_factor,
            paused: state.paused,
            resume_pending: state.resume_pending,
            stalled: state.stalled,
        })
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut ClockState) -> R) -> R {
        // poison 恢复：持锁线程 panic 后沿用内层状态（时钟在渲染帧内读取，
        // panic 传染只会把单次故障放大为持续崩溃；旧值优于崩溃）。
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut state)
    }

    fn calibrate_at(state: &mut ClockState, time_pos: f64, now: Instant) {
        state.time_pos = if time_pos.is_finite() {
            time_pos.max(0.0)
        } else {
            0.0
        };
        state.wall_at_calib = now;
        state.resume_pending = false;
        state.speed_factor = 1.0;
    }

    fn calibrate_smooth_at(state: &mut ClockState, time_pos: f64, now: Instant) {
        if state.paused || !time_pos.is_finite() {
            return;
        }
        // 恢复期首批事件：硬锚解冻，外推从事件值（≈暂停点）起步，
        // 不走软校准（避免与冻结值算 delta 触发回拉）。
        if state.resume_pending {
            Self::calibrate_at(state, time_pos, now);
            return;
        }
        if state.stalled {
            // 停滞期间外推冻结，事件值直接硬锚（不与冻结值算 delta）。
            Self::calibrate_at(state, time_pos, now);
            return;
        }
        let time_pos = time_pos.max(0.0);
        let predicted = Self::media_time_at(state, now);
        let delta = time_pos - predicted;
        let deviation = delta.abs();
        if deviation <= SMOOTH_DEAD_ZONE {
            // 偏差已消化到死区：先按旧速率推进锚点到当前时刻，再恢复
            // factor=1.0（R07：不推进直接改因子会把新因子应用到整个锚点
            // 以来的时间，造成额外前跳）。
            Self::advance_anchor_at(state, now);
            state.speed_factor = 1.0;
            return;
        }
        if deviation >= JUMP_THRESHOLD {
            Self::calibrate_at(state, time_pos, now);
            return;
        }
        if delta < 0.0 {
            // 事件滞后于外推：锚到当前外推值（media_time 不回撤），
            // 降速让外推慢下来、事件值追上来，偏差在后续事件中消化。
            // 降速幅度随偏差占比增长，最多压到 (1 − SLOWDOWN_MAX)。
            state.time_pos = predicted;
            state.wall_at_calib = now;
            let ratio = (deviation / JUMP_THRESHOLD).min(1.0);
            state.speed_factor = 1.0 - ratio * SLOWDOWN_MAX;
        } else {
            // 事件超前于外推：半收敛前移锚点（无回撤），恢复正常速度。
            state.time_pos = predicted + delta * SMOOTH_CONVERGE;
            state.wall_at_calib = now;
            state.speed_factor = 1.0;
        }
    }

    fn set_speed_at(state: &mut ClockState, speed: f64, now: Instant) {
        let speed = if speed.is_finite() && speed >= 0.0 {
            speed.clamp(0.0, SPEED_MAX)
        } else {
            1.0
        };
        if speed == state.speed {
            return;
        }
        Self::advance_anchor_at(state, now);
        state.speed = speed;
        // 用户主动调速：不继承收敛区降速，恢复正常外推速率。
        state.speed_factor = 1.0;
    }

    fn set_paused_at(state: &mut ClockState, paused: bool, now: Instant) {
        if paused == state.paused {
            return;
        }
        if paused {
            Self::advance_anchor_at(state, now);
            state.resume_pending = false;
        } else {
            // 解冻外推延迟到首批 time-pos 事件：先冻结 media_time 在
            // 暂停点（resume_pending 让 media_time_at 返回 time_pos），
            // 避免 mpv 起播延迟期间外推跑前面再被校准回拉。
            state.wall_at_calib = now;
            state.resume_pending = true;
            state.speed_factor = 1.0;
        }
        state.paused = paused;
    }

    fn set_stalled_at(state: &mut ClockState, stalled: bool, now: Instant) {
        if stalled == state.stalled {
            return;
        }
        if stalled {
            Self::advance_anchor_at(state, now);
        } else {
            state.wall_at_calib = now;
        }
        state.stalled = stalled;
    }

    fn media_time_at(state: &ClockState, now: Instant) -> f64 {
        if state.paused || state.resume_pending || state.stalled {
            return state.time_pos;
        }
        let dt = now
            .saturating_duration_since(state.wall_at_calib)
            .as_secs_f64();
        state.time_pos + dt * state.speed * state.speed_factor
    }

    /// 把锚点推进到 `now`（播放中按当前有效速度累积媒体时间后重锚）。
    fn advance_anchor_at(state: &mut ClockState, now: Instant) {
        if !state.paused && !state.resume_pending && !state.stalled {
            let dt = now
                .saturating_duration_since(state.wall_at_calib)
                .as_secs_f64();
            state.time_pos += dt * state.speed * state.speed_factor;
        }
        state.wall_at_calib = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn calibrate_anchors_media_time() {
        let t0 = Instant::now();
        let mut state = test_state_at(t0);
        MediaClock::calibrate_at(&mut state, 10.0, t0);
        assert_eq!(MediaClock::media_time_at(&state, t0), 10.0);
    }

    #[test]
    fn interpolates_at_unit_speed() {
        let t0 = Instant::now();
        let mut state = test_state_at(t0);
        state.paused = false;
        MediaClock::calibrate_at(&mut state, 10.0, t0);
        assert!((MediaClock::media_time_at(&state, t0 + secs(1.5)) - 11.5).abs() < 1e-9);
    }

    #[test]
    fn interpolates_with_speed_multiplier() {
        let t0 = Instant::now();
        let mut state = test_state_at(t0);
        state.paused = false;
        MediaClock::calibrate_at(&mut state, 10.0, t0);
        MediaClock::set_speed_at(&mut state, 2.0, t0);
        assert!((MediaClock::media_time_at(&state, t0 + secs(1.0)) - 12.0).abs() < 1e-9);
    }

    #[test]
    fn paused_freezes_media_time() {
        let t0 = Instant::now();
        let mut state = test_state_at(t0);
        state.paused = false;
        MediaClock::calibrate_at(&mut state, 10.0, t0);
        MediaClock::set_paused_at(&mut state, true, t0 + secs(0.5));
        assert!((MediaClock::media_time_at(&state, t0 + secs(10.0)) - 10.5).abs() < 1e-9);
    }

    #[test]
    fn resume_reanchors_from_pause_point() {
        let t0 = Instant::now();
        let mut state = test_state_at(t0);
        state.paused = false;
        MediaClock::calibrate_at(&mut state, 10.0, t0);
        MediaClock::set_paused_at(&mut state, true, t0 + secs(1.0));
        MediaClock::set_paused_at(&mut state, false, t0 + secs(2.0));
        // 恢复后冻结在暂停点 11.0，直到首批事件解冻外推。
        assert_eq!(MediaClock::media_time_at(&state, t0 + secs(2.5)), 11.0);
        MediaClock::calibrate_smooth_at(&mut state, 11.0, t0 + secs(2.5));
        assert!((MediaClock::media_time_at(&state, t0 + secs(3.0)) - 11.5).abs() < 1e-9);
    }

    #[test]
    fn speed_change_advances_anchor_first() {
        let t0 = Instant::now();
        let mut state = test_state_at(t0);
        state.paused = false;
        MediaClock::calibrate_at(&mut state, 10.0, t0);
        MediaClock::set_speed_at(&mut state, 2.0, t0 + secs(1.0));
        assert!((MediaClock::media_time_at(&state, t0 + secs(1.5)) - 12.0).abs() < 1e-9);
    }

    #[test]
    fn seek_recalibrates_immediately() {
        let t0 = Instant::now();
        let mut state = test_state_at(t0);
        state.paused = false;
        MediaClock::calibrate_at(&mut state, 10.0, t0);
        MediaClock::calibrate_at(&mut state, 100.0, t0 + secs(1.2));
        assert!((MediaClock::media_time_at(&state, t0 + secs(1.5)) - 100.3).abs() < 1e-9);
    }

    #[test]
    fn smooth_calibrate_ignores_dead_zone_deviation() {
        let t0 = Instant::now();
        let mut state = test_state_at(t0);
        state.paused = false;
        MediaClock::calibrate_at(&mut state, 10.0, t0);
        // 外推 10.2，事件值 10.16（滞后 40ms 链路抖动）→ 死区内不动锚。
        MediaClock::calibrate_smooth_at(&mut state, 10.16, t0 + secs(0.2));
        assert!((MediaClock::media_time_at(&state, t0 + secs(0.2)) - 10.2).abs() < 1e-9);
    }

    #[test]
    fn smooth_calibrate_converges_halfway() {
        let t0 = Instant::now();
        let mut state = test_state_at(t0);
        state.paused = false;
        MediaClock::calibrate_at(&mut state, 10.0, t0);
        // 外推 10.4，事件值 10.52（偏差 120ms）→ 锚定到 10.4+0.06。
        MediaClock::calibrate_smooth_at(&mut state, 10.52, t0 + secs(0.4));
        assert!((MediaClock::media_time_at(&state, t0 + secs(0.4)) - 10.46).abs() < 1e-9);
    }

    #[test]
    fn smooth_calibrate_snaps_large_deviation() {
        let t0 = Instant::now();
        let mut state = test_state_at(t0);
        state.paused = false;
        MediaClock::calibrate_at(&mut state, 10.0, t0);
        // 偏差 3s ≥ 阈值：真实跳变，硬锚。
        MediaClock::calibrate_smooth_at(&mut state, 13.0, t0 + secs(0.4));
        assert!((MediaClock::media_time_at(&state, t0 + secs(0.4)) - 13.0).abs() < 1e-9);
    }

    #[test]
    fn smooth_calibrate_keeps_pause_frozen() {
        let t0 = Instant::now();
        let mut state = test_state_at(t0);
        state.paused = true;
        MediaClock::calibrate_at(&mut state, 10.0, t0);
        // 暂停中的异常超前事件不得破坏冻结。
        MediaClock::calibrate_smooth_at(&mut state, 10.2, t0 + secs(0.4));
        assert_eq!(MediaClock::media_time_at(&state, t0 + secs(1.0)), 10.0);
    }

    #[test]
    fn smooth_calibrate_kills_sawtooth_from_jittered_events() {
        // debug 回归复现：60Hz time-pos 事件带 ±20ms 链路抖动时，
        // 硬锚定每事件回拉（media_time 非单调）；软校准保持单调。
        let t0 = Instant::now();
        let lag_pattern = [0.015, -0.01, 0.02, 0.0, 0.01, -0.015];
        for mode in 0..2 {
            let mut state = test_state_at(t0);
            state.paused = false;
            MediaClock::calibrate_at(&mut state, 10.0, t0);
            let mut prev = 10.0;
            let mut regressions = 0;
            for i in 1..=120 {
                let t = t0 + secs(i as f64 / 60.0);
                let true_media = 10.0 + i as f64 / 60.0;
                let lag = 0.02 + lag_pattern[(i - 1) % 6];
                let v = true_media - lag;
                if mode == 0 {
                    MediaClock::calibrate_at(&mut state, v, t);
                } else {
                    MediaClock::calibrate_smooth_at(&mut state, v, t);
                }
                let m = MediaClock::media_time_at(&state, t);
                if m < prev {
                    regressions += 1;
                }
                prev = m;
            }
            if mode == 0 {
                assert!(regressions > 0, "硬锚定应出现回撤（复现 bug）");
            } else {
                assert_eq!(regressions, 0, "软校准不应有回撤");
            }
        }
    }

    #[test]
    fn speed_is_clamped_to_max() {
        let t0 = Instant::now();
        let mut state = test_state_at(t0);
        state.paused = false;
        MediaClock::set_speed_at(&mut state, 99.0, t0);
        assert_eq!(state.speed, SPEED_MAX);
        MediaClock::set_speed_at(&mut state, -1.0, t0);
        assert_eq!(state.speed, 1.0);
    }

    #[test]
    fn resume_freezes_until_first_event() {
        let t0 = Instant::now();
        let mut state = test_state_at(t0);
        state.paused = false;
        MediaClock::calibrate_at(&mut state, 10.0, t0);
        MediaClock::set_paused_at(&mut state, true, t0 + secs(1.0));
        // 恢复：media_time 冻结在暂停点 11.0，不外推。
        MediaClock::set_paused_at(&mut state, false, t0 + secs(2.0));
        assert!(state.resume_pending);
        assert_eq!(MediaClock::media_time_at(&state, t0 + secs(2.5)), 11.0);
        // 首批事件到达：硬锚解冻。
        MediaClock::calibrate_smooth_at(&mut state, 11.02, t0 + secs(2.6));
        assert!(!state.resume_pending);
        assert!((MediaClock::media_time_at(&state, t0 + secs(2.6)) - 11.02).abs() < 1e-9);
    }

    #[test]
    fn smooth_calibrate_no_regression_on_lag() {
        // 事件滞后于外推（delta<0）：media_time 不回撤，锚到 predicted。
        let t0 = Instant::now();
        let mut state = test_state_at(t0);
        state.paused = false;
        MediaClock::calibrate_at(&mut state, 10.0, t0);
        // 外推到 10.4，事件值 10.28（滞后 120ms）→ 单向化锚到 10.4。
        let before = MediaClock::media_time_at(&state, t0 + secs(0.4));
        MediaClock::calibrate_smooth_at(&mut state, 10.28, t0 + secs(0.4));
        let after = MediaClock::media_time_at(&state, t0 + secs(0.4));
        assert!((after - before).abs() < 1e-9, "滞后事件不应回撤 media_time");
        assert!(state.speed_factor < 1.0, "滞后应触发降速");
    }

    #[test]
    fn smooth_calibrate_slowdown_digests_lag() {
        // 恒定 120ms 滞移（display-resample 漂移）：降速把稳态偏差压到
        // 死区边界附近振荡，media_time 严格单调无回撤且偏差不发散。
        let t0 = Instant::now();
        let mut state = test_state_at(t0);
        state.paused = false;
        MediaClock::calibrate_at(&mut state, 10.0, t0);
        const LAG: f64 = 0.120;
        let mut prev = 10.0;
        let mut max_dev = 0.0_f64;
        for i in 1..=100 {
            let t = t0 + secs(i as f64 / 10.0);
            let true_media = 10.0 + i as f64 / 10.0;
            MediaClock::calibrate_smooth_at(&mut state, true_media - LAG, t);
            let m = MediaClock::media_time_at(&state, t);
            assert!(m >= prev - 1e-9, "media_time 应单调，第 {i} 帧 {m} < {prev}");
            max_dev = max_dev.max((m - true_media).abs());
            prev = m;
        }
        assert!(max_dev < JUMP_THRESHOLD, "偏差应被降速钳制不发散，实际 max_dev={max_dev}");
    }

    #[test]
    fn wall_clock_backwards_is_defended() {
        let t0 = Instant::now();
        let mut state = test_state_at(t0);
        state.paused = false;
        MediaClock::calibrate_at(&mut state, 10.0, t0 + secs(5.0));
        assert_eq!(MediaClock::media_time_at(&state, t0), 10.0);
    }

    #[test]
    fn zero_speed_keeps_time_still() {
        let t0 = Instant::now();
        let mut state = test_state_at(t0);
        state.paused = false;
        MediaClock::calibrate_at(&mut state, 10.0, t0);
        MediaClock::set_speed_at(&mut state, 0.0, t0);
        assert_eq!(MediaClock::media_time_at(&state, t0 + secs(3.0)), 10.0);
    }

    #[test]
    fn invalid_inputs_fall_back_safely() {
        let t0 = Instant::now();
        let mut state = test_state_at(t0);
        state.paused = false;
        MediaClock::calibrate_at(&mut state, f64::NAN, t0);
        assert_eq!(MediaClock::media_time_at(&state, t0), 0.0);
        MediaClock::calibrate_at(&mut state, -5.0, t0);
        assert_eq!(MediaClock::media_time_at(&state, t0), 0.0);
        MediaClock::set_speed_at(&mut state, f64::NAN, t0);
        assert_eq!(state.speed, 1.0);
    }

    #[test]
    fn initial_state_is_paused_at_zero() {
        let clock = MediaClock::new();
        assert_eq!(clock.now(), 0.0);
    }

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    fn test_state_at(t0: Instant) -> ClockState {
        ClockState {
            time_pos: 0.0,
            wall_at_calib: t0,
            speed: 1.0,
            paused: true,
            resume_pending: false,
            speed_factor: 1.0,
            stalled: false,
        }
    }

    /// ClockSnapshot（R08）跨线程外推契约：覆盖三冻结态 + 正常外推公式。
    /// 覆盖层读 snapshot 自行外推，须与 MediaClock::media_time_at 同公式。
    #[test]
    fn clock_snapshot_normal_extrapolation() {
        let t0 = Instant::now();
        let snap = ClockSnapshot {
            time_pos: 10.0,
            wall_at_calib: t0,
            speed: 2.0,
            speed_factor: 0.5,
            paused: false,
            resume_pending: false,
            stalled: false,
        };
        // 1s 后：10 + 1*2*0.5 = 11
        assert!((snap.media_time_at(t0 + secs(1.0)) - 11.0).abs() < 1e-9);
    }

    #[test]
    fn clock_snapshot_paused_freezes() {
        let t0 = Instant::now();
        let snap = ClockSnapshot {
            time_pos: 10.0,
            wall_at_calib: t0,
            speed: 1.0,
            speed_factor: 1.0,
            paused: true,
            resume_pending: false,
            stalled: false,
        };
        assert_eq!(snap.media_time_at(t0 + secs(5.0)), 10.0);
    }

    #[test]
    fn clock_snapshot_stalled_freezes() {
        let t0 = Instant::now();
        let snap = ClockSnapshot {
            time_pos: 10.0,
            wall_at_calib: t0,
            speed: 1.0,
            speed_factor: 1.0,
            paused: false,
            resume_pending: false,
            stalled: true,
        };
        assert_eq!(snap.media_time_at(t0 + secs(5.0)), 10.0);
    }

    #[test]
    fn clock_snapshot_resume_pending_freezes() {
        let t0 = Instant::now();
        let snap = ClockSnapshot {
            time_pos: 10.0,
            wall_at_calib: t0,
            speed: 1.0,
            speed_factor: 1.0,
            paused: false,
            resume_pending: true,
            stalled: false,
        };
        assert_eq!(snap.media_time_at(t0 + secs(5.0)), 10.0);
    }

    #[test]
    fn clock_snapshot_default_is_paused_frozen() {
        let snap = ClockSnapshot::default();
        assert!(snap.paused);
        assert_eq!(snap.media_time_at(Instant::now()), 0.0);
    }

    #[test]
    fn clock_snapshot_matches_media_clock_at() {
        // snapshot 外推须与 MediaClock::media_time_at 同公式（R08 契约）。
        let t0 = Instant::now();
        let state = ClockState {
            time_pos: 10.0,
            wall_at_calib: t0,
            speed: 1.5,
            paused: false,
            resume_pending: false,
            speed_factor: 0.8,
            stalled: false,
        };
        let snap = ClockSnapshot {
            time_pos: state.time_pos,
            wall_at_calib: state.wall_at_calib,
            speed: state.speed,
            speed_factor: state.speed_factor,
            paused: state.paused,
            resume_pending: state.resume_pending,
            stalled: state.stalled,
        };
        let t = t0 + secs(2.0);
        assert!((snap.media_time_at(t) - MediaClock::media_time_at(&state, t)).abs() < 1e-9);
    }
}
