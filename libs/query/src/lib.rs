pub struct Client {}

impl Client {
    pub fn new() -> Self {
        Self {}
    }
}

pub mod http {
    include!(concat!(env!("OUT_DIR"), "/fk_app.rs"));
}

#[cfg(test)]
mod tests {
    // use super::*;
}
