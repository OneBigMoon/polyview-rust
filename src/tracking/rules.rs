use super::*;
use crate::catalog::Episode;

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Rules {
    quality: String,
    include_keywords: Vec<String>,
    exclude_keywords: Vec<String>,
}

impl Rules {
    pub(super) fn encode(&mut self) -> Result<String, AppError> {
        if self.include_keywords.len() > 32 || self.exclude_keywords.len() > 32 {
            return Err(AppError::bad_request("每类订阅关键词最多 32 个"));
        }
        for term in std::iter::once(&mut self.quality)
            .chain(&mut self.include_keywords)
            .chain(&mut self.exclude_keywords)
        {
            if term.chars().count() > 80 || term.chars().any(char::is_control) {
                return Err(AppError::bad_request("订阅关键词须为 80 字以内的单行文本"));
            }
            *term = term.trim().to_lowercase();
        }
        self.include_keywords.retain(|word| !word.is_empty());
        self.exclude_keywords.retain(|word| !word.is_empty());
        serde_json::to_string(self).map_err(|error| AppError::internal(error.to_string()))
    }

    pub(super) fn read(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Self> {
        let raw = row.get_ref(index)?.as_str()?;
        serde_json::from_str(raw).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    }

    pub(super) fn active(&self) -> bool {
        !self.quality.is_empty()
            || !self.include_keywords.is_empty()
            || !self.exclude_keywords.is_empty()
    }

    pub(super) fn matches(
        &self,
        detail: &CatalogDetail,
        group: &PlayGroup,
        episode: &Episode,
    ) -> bool {
        if !self.active() {
            return true;
        }
        // Ignore signed query strings. Quality and release names often occur in the URL path.
        let path = url::Url::parse(&episode.url).ok();
        let text = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            detail.title,
            detail.remark,
            detail.source_name,
            group.name,
            episode.name,
            path.as_ref().map_or("", url::Url::path)
        )
        .to_lowercase();
        let quality = match self.quality.as_str() {
            "4k" | "2160p" | "uhd" => ["4k", "2160p", "uhd"]
                .iter()
                .any(|word| text.contains(word)),
            "8k" | "4320p" => text.contains("8k") || text.contains("4320p"),
            other => text.contains(other),
        };
        quality
            && self.include_keywords.iter().all(|word| text.contains(word))
            && !self.exclude_keywords.iter().any(|word| text.contains(word))
    }
}
