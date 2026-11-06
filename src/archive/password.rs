use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::Result;

use super::{Cancelled, JobControl};

type Prompt = dyn Fn(&Path, bool, &JobControl) -> Result<String> + Send + Sync;

/// Passwords are kept only in memory and scoped to one application window.
#[derive(Clone)]
pub struct PasswordPrompt {
    prompt: Arc<Prompt>,
    passwords: Arc<Mutex<HashMap<PathBuf, String>>>,
    request: Arc<Mutex<()>>,
}

impl PasswordPrompt {
    pub fn new(
        prompt: impl Fn(&Path, bool, &JobControl) -> Result<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            prompt: Arc::new(prompt),
            passwords: Arc::default(),
            request: Arc::default(),
        }
    }

    fn cached(&self, path: &Path) -> Option<String> {
        self.passwords
            .lock()
            .expect("password cache poisoned")
            .get(path)
            .cloned()
    }

    fn ask(&self, path: &Path, retry: bool, control: &JobControl) -> Result<String> {
        let _request = self.request.lock().expect("password request poisoned");
        control.check_cancelled()?;
        if !retry && let Some(password) = self.cached(path) {
            return Ok(password);
        }
        let password = (self.prompt)(path, retry, control)?;
        control.check_cancelled()?;
        if password.is_empty() {
            return Err(Cancelled.into());
        }
        self.passwords
            .lock()
            .expect("password cache poisoned")
            .insert(path.to_path_buf(), password.clone());
        Ok(password)
    }
}

#[derive(Debug)]
pub(super) enum PasswordError {
    Required,
    Incorrect,
}

impl std::fmt::Display for PasswordError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Required => "压缩包需要密码",
            Self::Incorrect => "压缩包密码不正确",
        })
    }
}

impl std::error::Error for PasswordError {}

fn password_error(error: &anyhow::Error, encrypted: bool) -> bool {
    error.chain().any(|cause| {
        cause.is::<PasswordError>()
            || matches!(
                cause.downcast_ref::<zip::result::ZipError>(),
                Some(zip::result::ZipError::InvalidPassword)
                    | Some(zip::result::ZipError::UnsupportedArchive(
                        zip::result::ZipError::PASSWORD_REQUIRED
                    ))
            )
            || matches!(
                cause.downcast_ref::<sevenz_rust2::Error>(),
                Some(
                    sevenz_rust2::Error::PasswordRequired
                        | sevenz_rust2::Error::MaybeBadPassword(_)
                )
            )
            || (encrypted
                && cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
                    matches!(
                        error.kind(),
                        std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof
                    ) || error.get_ref().is_some_and(|inner| {
                        matches!(
                            inner.downcast_ref::<sevenz_rust2::Error>(),
                            Some(sevenz_rust2::Error::ChecksumVerificationFailed)
                        )
                    })
                }))
    })
}

impl JobControl {
    pub fn with_password_prompt(mut self, prompt: PasswordPrompt) -> Self {
        self.password_prompt = Some(prompt);
        self
    }

    pub(super) fn with_archive_password<T>(
        &self,
        path: &Path,
        supplied: Option<&str>,
        required: bool,
        retry_safe: bool,
        mut operation: impl FnMut(Option<&str>) -> Result<T>,
    ) -> Result<T> {
        let Some(prompt) = &self.password_prompt else {
            return operation(supplied);
        };
        let mut password = supplied
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or_else(|| prompt.cached(path));
        if required && password.is_none() {
            password = Some(prompt.ask(path, false, self)?);
        }
        loop {
            self.check_cancelled()?;
            match operation(password.as_deref()) {
                Err(error) if password_error(&error, required) => {
                    prompt
                        .passwords
                        .lock()
                        .expect("password cache poisoned")
                        .remove(path);
                    if !retry_safe {
                        return Err(error);
                    }
                    password = Some(prompt.ask(path, password.is_some(), self)?);
                }
                result => return result,
            }
        }
    }
}
