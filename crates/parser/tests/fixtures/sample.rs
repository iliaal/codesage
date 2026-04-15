use std::collections::HashMap;
use std::io::Read;

const MAX_SIZE: usize = 1024;

static GLOBAL_NAME: &str = "codesage";

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub mod utils {
    pub fn helper() -> String {
        String::new()
    }
}

pub struct Config {
    pub debug: bool,
    pub name: String,
}

pub enum LogLevel {
    Debug,
    Info,
    Error,
}

pub trait Serializable {
    fn serialize(&self) -> String;
}

impl Config {
    pub fn new(name: String) -> Self {
        Config {
            debug: false,
            name,
        }
    }

    pub fn with_debug(mut self) -> Self {
        self.debug = true;
        self
    }
}

impl Serializable for Config {
    fn serialize(&self) -> String {
        format!("{}:{}", self.name, self.debug)
    }
}

macro_rules! log_msg {
    ($msg:expr) => {
        println!("{}", $msg);
    };
}

pub fn process(config: &Config) -> Result<()> {
    log_msg!("processing");
    let _map: HashMap<String, String> = HashMap::new();
    Ok(())
}
