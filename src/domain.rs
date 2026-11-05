use std::time::Instant;

#[derive(Debug)]
pub struct Target {
    pub url: String,
    pub proxy: Option<String>,
    pub deadline: Instant,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NamedCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub expires: Option<f64>,
    pub http_only: bool,
    pub secure: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cleared {
    pub url: String,
    pub status: u16,
    pub body: String,
    pub cookies: Vec<NamedCookie>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolveError {
    NotChallenged,
    Blocked(u16),
    Network(String),
    Rejected(String),
    Challenged(u16),
    Unsupported(String),
    Config(String),
}
