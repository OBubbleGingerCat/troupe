use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerStartErrorCode {
    InvalidConfiguration,
    InvalidRoutes,
    BindFailed,
    ReadinessProbeFailed,
    ContextSpawnFailed,
    ContextInitializationFailed,
    ContextExitedBeforeReady,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerStartError {
    code: ServerStartErrorCode,
    message: String,
}

impl ServerStartError {
    pub(crate) fn new(code: ServerStartErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub const fn code(&self) -> ServerStartErrorCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ServerStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ServerStartError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerCoreFailureCode {
    ListenerFailed,
    ExecutionContextExited,
    ExecutionContextPanicked,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerCoreFailure {
    code: ServerCoreFailureCode,
    message: String,
}

impl ServerCoreFailure {
    pub(crate) fn new(code: ServerCoreFailureCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub const fn code(&self) -> ServerCoreFailureCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ServerCoreFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ServerCoreFailure {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestError {
    code: String,
    message: String,
}

impl RequestError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        let code = code.into();
        Self {
            code: if code.is_empty() {
                "request_failed".to_owned()
            } else {
                code
            },
            message: message.into(),
        }
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for RequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RequestError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteConfigurationError(&'static str);

impl RouteConfigurationError {
    pub(crate) const fn new(message: &'static str) -> Self {
        Self(message)
    }
}

impl fmt::Display for RouteConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for RouteConfigurationError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerShutdownError {
    ExecutionContextPanicked,
}

impl fmt::Display for ServerShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExecutionContextPanicked => {
                formatter.write_str("diagnostic server execution context panicked")
            }
        }
    }
}

impl std::error::Error for ServerShutdownError {}
