use serde::Deserialize;
use std::collections::HashMap;
use std::fmt;

/// A coarse file kind derived from an Entry's extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Category {
    Video,
    Image,
    Audio,
    Archive,
    Code,
    Document,
    Executable,
    Other,
}

impl Category {
    pub const ALL: [Category; 8] = [
        Category::Video,
        Category::Image,
        Category::Audio,
        Category::Archive,
        Category::Code,
        Category::Document,
        Category::Executable,
        Category::Other,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Category::Video => "Video",
            Category::Image => "Image",
            Category::Audio => "Audio",
            Category::Archive => "Archive",
            Category::Code => "Code",
            Category::Document => "Document",
            Category::Executable => "Executable",
            Category::Other => "Other",
        }
    }

    /// Parses a Category name, ignoring case.
    pub fn parse(name: &str) -> Option<Category> {
        Category::ALL
            .into_iter()
            .find(|c| c.name().eq_ignore_ascii_case(name))
    }
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

const BUILTIN: &[(Category, &[&str])] = &[
    (
        Category::Video,
        &[
            "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "mpg", "mpeg", "3gp", "m2ts",
            "vob", "ogv",
        ],
    ),
    (
        Category::Image,
        &[
            "jpg", "jpeg", "png", "gif", "bmp", "tif", "tiff", "webp", "heic", "heif", "svg",
            "ico", "raw", "cr2", "nef", "arw", "dng", "psd", "xcf", "avif", "jxl",
        ],
    ),
    (
        Category::Audio,
        &[
            "mp3", "flac", "wav", "aac", "ogg", "opus", "m4a", "wma", "aiff", "alac", "mid", "midi",
        ],
    ),
    (
        Category::Archive,
        &[
            "zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz", "zst", "lz4", "lzma", "cab",
            "iso", "img", "dmg", "deb", "rpm", "jar", "apk", "appimage", "pkg", "vhd", "vhdx",
            "vmdk", "qcow2", "wim",
        ],
    ),
    (
        Category::Code,
        &[
            "rs", "c", "h", "cpp", "hpp", "cc", "cs", "java", "kt", "go", "py", "js", "mjs", "ts",
            "tsx", "jsx", "rb", "php", "swift", "m", "lua", "sh", "bash", "zsh", "ps1", "html",
            "css", "scss", "json", "toml", "yaml", "yml", "xml", "sql", "o", "obj", "a", "lib",
            "rlib", "pyc", "class", "wasm",
        ],
    ),
    (
        Category::Document,
        &[
            "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "rtf", "txt",
            "md", "csv", "epub", "mobi", "tex", "log",
        ],
    ),
    (
        Category::Executable,
        &[
            "exe", "dll", "so", "dylib", "msi", "bin", "sys", "elf", "app", "com", "bat", "cmd",
        ],
    ),
];

/// User overrides, as written in `categories.toml`:
///
/// ```toml
/// [extensions]
/// psd = "Document"
/// blend = "Image"
/// ```
#[derive(Debug, Default, Deserialize)]
pub struct CategoryOverrides {
    #[serde(default)]
    pub extensions: HashMap<String, String>,
}

/// Maps extensions to Categories. Unknown extensions, and names without one, are `Other`.
#[derive(Clone, Debug)]
pub struct CategoryMap {
    by_extension: HashMap<String, Category>,
}

impl Default for CategoryMap {
    fn default() -> Self {
        Self::builtin()
    }
}

impl CategoryMap {
    pub fn builtin() -> Self {
        let mut by_extension = HashMap::new();
        for (category, extensions) in BUILTIN {
            for ext in *extensions {
                // An extension listed twice keeps its first Category.
                by_extension.entry((*ext).to_owned()).or_insert(*category);
            }
        }
        Self { by_extension }
    }

    /// The built-in map with user overrides applied. Fails on an unknown Category name.
    pub fn with_overrides(overrides: &CategoryOverrides) -> Result<Self, String> {
        let mut map = Self::builtin();
        for (ext, name) in &overrides.extensions {
            let category = Category::parse(name)
                .ok_or_else(|| format!("unknown Category `{name}` for extension `{ext}`"))?;
            map.by_extension
                .insert(ext.trim_start_matches('.').to_ascii_lowercase(), category);
        }
        Ok(map)
    }

    /// Parses `categories.toml` text and applies it to the built-in map.
    pub fn from_toml(text: &str) -> Result<Self, String> {
        let overrides: CategoryOverrides = toml::from_str(text).map_err(|e| e.to_string())?;
        Self::with_overrides(&overrides)
    }

    pub fn of_extension(&self, ext: &str) -> Category {
        if ext.bytes().all(|b| !b.is_ascii_uppercase()) {
            return self
                .by_extension
                .get(ext)
                .copied()
                .unwrap_or(Category::Other);
        }
        self.by_extension
            .get(&ext.to_ascii_lowercase())
            .copied()
            .unwrap_or(Category::Other)
    }

    pub fn of_name(&self, name: &str) -> Category {
        extension(name).map_or(Category::Other, |ext| self.of_extension(ext))
    }

    /// Every extension mapped to `category`.
    pub fn extensions_of(&self, category: Category) -> impl Iterator<Item = &str> {
        self.by_extension
            .iter()
            .filter(move |(_, c)| **c == category)
            .map(|(e, _)| e.as_str())
    }
}

/// The extension of a file name, without the dot. Names like `.bashrc` have none.
pub fn extension(name: &str) -> Option<&str> {
    let dot = name.rfind('.')?;
    (dot > 0 && dot + 1 < name.len()).then(|| &name[dot + 1..])
}
