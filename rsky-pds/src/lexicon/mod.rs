use crate::lexicon::lexicons::Root;
use lazy_static::lazy_static;

lazy_static! {
    // Deserializing Root needs more stack than tokio worker and test
    // threads provide, so parse on a dedicated thread
    pub static ref LEXICONS: Box<Root> = std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            Box::new(
                toml::from_str::<Root>(include_str!("lexicons.toml"))
                    .expect("Failed to deserialize lexicons.toml"),
            )
        })
        .expect("Failed to spawn lexicon parser thread")
        .join()
        .expect("Lexicon parser thread panicked");
}

pub mod lexicons;

#[cfg(test)]
mod tests {
    use super::LEXICONS;

    #[test]
    fn loads_lexicons_without_runtime_filesystem_dependency() {
        std::thread::Builder::new()
            .name("lexicons-load".to_string())
            .stack_size(32 * 1024 * 1024)
            .spawn(|| {
                let _ = &LEXICONS.com_atproto_repo_put_record;
            })
            .expect("failed to spawn lexicon loader thread")
            .join()
            .expect("lexicon loader thread panicked");
    }
}
