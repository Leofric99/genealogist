use std::path::PathBuf;

pub const DEFAULT_SYSTEM_PROMPT: &str = r#"You are a meticulous historical-record transcription engine. Extract 100% of all visible information from the supplied record; never summarize, omit, or silently normalize any text. Preserve spelling and uncertainty, and mark illegible text as [illegible] or [unclear: best reading]. Include every column, field label, signature, stamp, address, monetary sum, and marginal note.

Return only Markdown beginning with a YAML frontmatter block delimited by --- lines. Include document_type, date_recorded, location_recorded, primary_individuals, all_extracted_dates, and transcription_confidence. Represent primary_individuals as a sequence of mappings with name, role, age, occupation, and birthplace keys; use null for unavailable values. Represent all_extracted_dates as a sequence preserving each visible date and its label/context. Use a numeric confidence from 0 to 1 when possible. Quote YAML strings safely. In the Markdown body, format tabular data as complete Markdown tables, pre-printed forms as key-value bullet lists, and running prose as paragraphs. Render seals and marginal notes as blockquotes. Keep original units, punctuation, and line order where useful. Do not invent facts."#;

#[derive(Clone, Debug)]
pub struct Config {
    pub database_path: PathBuf,
    pub records_dir: PathBuf,
    pub output_dir: PathBuf,
    pub bind_addr: String,
    pub gemini_api_key: Option<String>,
    pub gemini_model: String,
    pub validation_model: String,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            database_path: std::env::var_os("DATABASE_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("./data/genealogy.sqlite3")),
            records_dir: std::env::var_os("RECORDS_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/data/records")),
            output_dir: std::env::var_os("OUTPUT_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/data/output")),
            bind_addr: std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_owned()),
            gemini_api_key: std::env::var("GEMINI_API_KEY").ok(),
            gemini_model: std::env::var("GEMINI_MODEL")
                .unwrap_or_else(|_| "gemini-2.5-flash".to_owned()),
            validation_model: std::env::var("GEMINI_VALIDATION_MODEL")
                .unwrap_or_else(|_| "gemini-2.5-flash-lite".to_owned()),
        }
    }
}
