use crate::{
    DictionaryMode, PostProcessError, PostProcessor, ProcessedText, ProcessingContext, Transcript,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct DeterministicPostProcessor;

impl PostProcessor for DeterministicPostProcessor {
    fn process(
        &self,
        transcript: Transcript,
        context: &ProcessingContext,
    ) -> Result<ProcessedText, PostProcessError> {
        let mut text = transcript.text.trim().to_owned();

        if context.normalize_spaces {
            text = normalize_spaces(&text, context.preserve_newlines);
        }

        for entry in &context.dictionary {
            if entry.mode == DictionaryMode::Exact && text == entry.spoken {
                text = entry.replacement.clone();
            }
        }

        if context.append_newline && !text.is_empty() {
            text.push('\n');
        }

        if context.append_space && !text.is_empty() {
            text.push(' ');
        }

        Ok(ProcessedText {
            text,
            source: transcript,
        })
    }
}

fn normalize_spaces(input: &str, preserve_newlines: bool) -> String {
    let mut normalized = String::with_capacity(input.len());
    let mut pending_space = false;

    for character in input.chars() {
        if character == '\n' && preserve_newlines {
            while normalized.ends_with(' ') {
                normalized.pop();
            }
            normalized.push('\n');
            pending_space = false;
        } else if character.is_whitespace() {
            pending_space = true;
        } else {
            if pending_space && !normalized.is_empty() && !normalized.ends_with('\n') {
                normalized.push(' ');
            }
            normalized.push(character);
            pending_space = false;
        }
    }

    normalized.trim().to_owned()
}
