//! Small contracts shared by workspace processes.

pub mod cli_error {
    /// A readable command failure with the process exit status to return.
    #[derive(Debug)]
    pub struct CliError {
        pub message: String,
        pub exit_code: i32,
    }

    impl CliError {
        pub fn failure(message: impl Into<String>) -> Self {
            Self {
                message: message.into(),
                exit_code: 1,
            }
        }

        pub fn usage(message: impl Into<String>) -> Self {
            Self {
                message: message.into(),
                exit_code: 2,
            }
        }
    }

    pub type Result<T> = std::result::Result<T, CliError>;
}

pub mod api_error;
pub mod config;
