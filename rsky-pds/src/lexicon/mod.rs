use crate::lexicon::lexicons::Root;
use lazy_static::lazy_static;

lazy_static! {
    pub static ref LEXICONS: Root = {
        let toml_str = include_str!("lexicons.toml");
        let cargo_toml: Root =
            toml::from_str(toml_str).expect("Failed to deserialize lexicons.toml");
        cargo_toml
    };
}

pub mod lexicons;
