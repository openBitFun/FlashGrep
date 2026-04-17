use crate::AppError;

pub(crate) fn not_implemented(feature: &str) -> AppError {
    AppError::Protocol(format!("index engine {feature} is not implemented yet"))
}
