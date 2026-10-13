/// The value of a metadata entry — mirrors [`sx_plugin_api::SxMetaValue`].
#[derive(Debug, Clone)]
pub enum MetaValue {
    Text(String),
    Number(f64),
    Bytes(Vec<u8>),
}

/// A single metadata entry — mirrors [`sx_plugin_api::SxMetaEntry`].
#[derive(Debug, Clone)]
pub struct MetaEntry {
    pub key: String,
    pub value: MetaValue,
}

/// A single search result delivered to the UI — mirrors [`sx_plugin_api::SxResult`].
///
/// File hits use `namespace = "file"` and carry `file_path` in metadata.
#[derive(Debug, Clone)]
pub struct ResultMsg {
    pub namespace: String,
    pub score: f64,
    pub title: Option<String>,
    pub metadata: Vec<MetaEntry>,
}

impl ResultMsg {
    /// Construct a plain file-hit result from a path and a normalised score.
    pub fn from_file(path: String, score: f64) -> Self {
        Self {
            namespace: "file".to_owned(),
            score,
            title: Some(path.clone()),
            metadata: vec![MetaEntry {
                key: "file_path".to_owned(),
                value: MetaValue::Text(path),
            }],
        }
    }

    /// Look up a text metadata value by key. Returns `None` for missing keys
    /// or non-`Text` values.
    pub fn meta(&self, key: &str) -> Option<&str> {
        self.metadata.iter().find(|e| e.key == key).and_then(|e| {
            if let MetaValue::Text(s) = &e.value {
                Some(s.as_str())
            } else {
                None
            }
        })
    }

    /// Look up a raw-bytes metadata value by key (e.g. `"icon_png"`).
    pub fn meta_bytes(&self, key: &str) -> Option<&[u8]> {
        self.metadata.iter().find(|e| e.key == key).and_then(|e| {
            if let MetaValue::Bytes(b) = &e.value {
                Some(b.as_slice())
            } else {
                None
            }
        })
    }

    /// Look up a numeric metadata value by key.
    pub fn meta_number(&self, key: &str) -> Option<f64> {
        self.metadata.iter().find(|e| e.key == key).and_then(|e| {
            if let MetaValue::Number(n) = &e.value {
                Some(*n)
            } else {
                None
            }
        })
    }
    /// Returns `true` when the result carries no displayable content.
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.metadata.is_empty()
    }
}

impl From<sx_plugin_api::SxResult> for ResultMsg {
    fn from(res: sx_plugin_api::SxResult) -> Self {
        let namespace = res.source_namespace.into_string();
        let score = res.score;
        let title = res.title.match_owned(|b| Some(b.into_string()), || None);

        let metadata: Vec<MetaEntry> = res
            .metadata
            .into_iter()
            .map(|e| {
                let key = e.key.into_string();
                let value = e.value.match_owned(
                    |t| MetaValue::Text(t.into_string()),
                    |n| MetaValue::Number(n),
                    |b| MetaValue::Bytes(b.into_vec()),
                );
                MetaEntry { key, value }
            })
            .collect();

        Self {
            namespace,
            score,
            title,
            metadata,
        }
    }
}
