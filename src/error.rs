use thiserror::Error;

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error(transparent)]
    Definition(#[from] crate::definition::DefinitionError),

    #[error(transparent)]
    Runtime(#[from] crate::just_runtime::RuntimeError),

    #[error(transparent)]
    Hook(#[from] crate::hooks::HookError),

    #[error(transparent)]
    Policy(#[from] crate::policy::PolicyError),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Message(String),
}

pub type Result<T> = std::result::Result<T, PipelineError>;
