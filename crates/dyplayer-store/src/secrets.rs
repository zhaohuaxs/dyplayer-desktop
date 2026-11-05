const SERVICE: &str = "dyplayer";

#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    #[error("keyring error: {0}")]
    Keyring(String),
}

pub fn entry_for_source(source_id: i64) -> Result<keyring::Entry, SecretsError> {
    keyring::Entry::new(SERVICE, &format!("source:{source_id}"))
        .map_err(|e| SecretsError::Keyring(e.to_string()))
}

pub fn set_source_credential(source_id: i64, secret: &str) -> Result<(), SecretsError> {
    let entry = entry_for_source(source_id)?;
    entry
        .set_password(secret)
        .map_err(|e| SecretsError::Keyring(e.to_string()))?;
    // 写后读回校验：防御后端"写入成功但数据丢弃"（keyring 3 缺平台 feature 时
    // 的 mock 后端即此行为），让故障在保存当场暴露而非浏览时 401。
    let readback = entry
        .get_password()
        .map_err(|e| SecretsError::Keyring(e.to_string()))?;
    if readback != secret {
        return Err(SecretsError::Keyring(
            "写入后读回不一致（凭据后端异常）".into(),
        ));
    }
    Ok(())
}

pub fn get_source_credential(source_id: i64) -> Result<Option<String>, SecretsError> {
    match entry_for_source(source_id)?.get_password() {
        Ok(s) => Ok(Some(s)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(SecretsError::Keyring(e.to_string())),
    }
}

pub fn delete_source_credential(source_id: i64) -> Result<(), SecretsError> {
    match entry_for_source(source_id)?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(SecretsError::Keyring(e.to_string())),
    }
}
