use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DocumentOrigin {
    File(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DocumentKind {
    OneCMetadataSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedDocument {
    pub id: String,
    pub title: String,
    pub kind: DocumentKind,
    pub origin: DocumentOrigin,
    pub source_paths: Vec<PathBuf>,
    pub language: String,
    pub content: String,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneCMetadataSummary {
    pub object_type: String,
    pub object_name: String,
    pub relative_path: PathBuf,
    pub title: String,
    pub module_links: Vec<(String, PathBuf)>,
    pub forms: Vec<String>,
    pub commands: Vec<String>,
    pub properties: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct OneCIngestor;

impl OneCIngestor {
    pub fn new() -> Self {
        Self
    }

    pub fn ingest_metadata_file(
        &self,
        repo_root: &Path,
        metadata_path: &Path,
    ) -> Result<Option<NormalizedDocument>> {
        let summary = match self.parse_metadata_summary(repo_root, metadata_path)? {
            Some(summary) => summary,
            None => return Ok(None),
        };

        let mut metadata = summary.properties.clone();
        metadata.insert("object_type".to_string(), summary.object_type.clone());
        metadata.insert("object_name".to_string(), summary.object_name.clone());
        metadata.insert(
            "relative_path".to_string(),
            summary.relative_path.to_string_lossy().to_string(),
        );

        Ok(Some(NormalizedDocument {
            id: synthetic_summary_id(&summary.relative_path),
            title: summary.title.clone(),
            kind: DocumentKind::OneCMetadataSummary,
            origin: DocumentOrigin::File(metadata_path.to_path_buf()),
            source_paths: collect_source_paths(metadata_path, &summary.module_links),
            language: "text".to_string(),
            content: render_summary_text(&summary),
            metadata,
        }))
    }

    pub fn parse_metadata_summary(
        &self,
        repo_root: &Path,
        metadata_path: &Path,
    ) -> Result<Option<OneCMetadataSummary>> {
        let xml = fs::read_to_string(metadata_path)
            .with_context(|| format!("Failed to read 1C metadata file {}", metadata_path.display()))?;
        let parsed = parse_metadata_xml(&xml)?;

        let object_type = match parsed.object_type {
            Some(object_type) => object_type,
            None => return Ok(None),
        };
        let object_name = match parsed.name {
            Some(name) if !name.is_empty() => name,
            _ => return Ok(None),
        };

        let relative_path = metadata_path
            .strip_prefix(repo_root)
            .unwrap_or(metadata_path)
            .to_path_buf();

        let module_links = discover_module_links(repo_root, metadata_path);
        let forms = discover_named_children(repo_root, metadata_path, "Forms");
        let commands = discover_named_children(repo_root, metadata_path, "Commands");

        let mut properties = parsed.properties;
        if let Some(synonym) = parsed.synonym {
            properties.insert("Synonym".to_string(), synonym);
        }

        Ok(Some(OneCMetadataSummary {
            title: format!("1C Object: {} {}", object_type, object_name),
            object_type,
            object_name,
            relative_path,
            module_links,
            forms,
            commands,
            properties,
        }))
    }
}

#[derive(Debug, Default)]
struct ParsedMetadata {
    object_type: Option<String>,
    name: Option<String>,
    synonym: Option<String>,
    properties: BTreeMap<String, String>,
}

fn parse_metadata_xml(xml: &str) -> Result<ParsedMetadata> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut parsed = ParsedMetadata::default();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                path.push(local_name(e.name().as_ref()));
                maybe_capture_object_type(&path, &mut parsed);
            }
            Ok(Event::Empty(e)) => {
                path.push(local_name(e.name().as_ref()));
                maybe_capture_object_type(&path, &mut parsed);
                path.pop();
            }
            Ok(Event::Text(e)) => {
                let text = e.decode()?.trim().to_string();
                if !text.is_empty() {
                    record_text(&path, &text, &mut parsed);
                }
            }
            Ok(Event::CData(e)) => {
                let text = e.decode()?.trim().to_string();
                if !text.is_empty() {
                    record_text(&path, &text, &mut parsed);
                }
            }
            Ok(Event::End(_)) => {
                path.pop();
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(err).context("Failed to parse 1C metadata XML"),
            _ => {}
        }

        buf.clear();
    }

    Ok(parsed)
}

fn maybe_capture_object_type(path: &[String], parsed: &mut ParsedMetadata) {
    if parsed.object_type.is_none() && path.len() == 2 && path[0] == "MetaDataObject" {
        parsed.object_type = Some(path[1].clone());
    }
}

fn record_text(path: &[String], text: &str, parsed: &mut ParsedMetadata) {
    if path.len() < 3 || path[0] != "MetaDataObject" {
        return;
    }

    let object_type = match parsed.object_type.as_deref() {
        Some(value) => value,
        None => return,
    };

    if path.get(1).map(String::as_str) != Some(object_type) {
        return;
    }

    let Some(last) = path.last() else {
        return;
    };

    if path.len() == 3 {
        if last == "Name" {
            parsed.name = Some(text.to_string());
            return;
        }

        if should_record_property(last) {
            parsed
                .properties
                .entry(last.clone())
                .or_insert_with(|| text.to_string());
        }
        return;
    }

    if path.len() == 4 && path[2] == "Synonym" && last == "item" && parsed.synonym.is_none() {
        parsed.synonym = Some(text.to_string());
    }
}

fn should_record_property(tag: &str) -> bool {
    matches!(
        tag,
        "Global"
            | "Server"
            | "ClientManagedApplication"
            | "ManagedApplication"
            | "ExternalConnection"
            | "OrdinaryApplication"
            | "UseStandardCommands"
            | "DefaultListForm"
            | "DefaultObjectForm"
    )
}

fn local_name(name: &[u8]) -> String {
    let full = String::from_utf8_lossy(name);
    full.rsplit(':').next().unwrap_or(full.as_ref()).to_string()
}

fn discover_module_links(repo_root: &Path, metadata_path: &Path) -> Vec<(String, PathBuf)> {
    let mut links = Vec::new();
    let Some(metadata_dir) = metadata_path.parent() else {
        return links;
    };

    for (label, candidate) in candidate_module_paths(metadata_path) {
        if candidate.is_file() {
            let relative = candidate
                .strip_prefix(repo_root)
                .unwrap_or(&candidate)
                .to_path_buf();
            links.push((label.to_string(), relative));
        }
    }

    if metadata_dir.ends_with("Forms") {
        let form_dir = metadata_dir.join(metadata_path.file_stem().unwrap_or_default());
        let fallback = form_dir.join("Ext").join("Module.bsl");
        if fallback.is_file() {
            let relative = fallback
                .strip_prefix(repo_root)
                .unwrap_or(&fallback)
                .to_path_buf();
            if !links.iter().any(|(_, path)| path == &relative) {
                links.push(("Module".to_string(), relative));
            }
        }
    }

    links
}

fn candidate_module_paths(metadata_path: &Path) -> Vec<(&'static str, PathBuf)> {
    let Some(metadata_dir) = metadata_path.parent() else {
        return Vec::new();
    };

    let stem = metadata_path.file_stem().unwrap_or_default();
    let form_dir = metadata_dir.join(stem);

    vec![
        ("Manager Module", metadata_dir.join("Ext").join("ManagerModule.bsl")),
        ("Object Module", metadata_dir.join("Ext").join("ObjectModule.bsl")),
        (
            "Record Set Module",
            metadata_dir.join("Ext").join("RecordSetModule.bsl"),
        ),
        (
            "Value Manager Module",
            metadata_dir.join("Ext").join("ValueManagerModule.bsl"),
        ),
        ("Module", metadata_dir.join("Ext").join("Module.bsl")),
        (
            "Form Module",
            form_dir.join("Ext").join("Form").join("Module.bsl"),
        ),
    ]
}

fn discover_named_children(repo_root: &Path, metadata_path: &Path, dir_name: &str) -> Vec<String> {
    let Some(metadata_dir) = metadata_path.parent() else {
        return Vec::new();
    };

    let child_dir = metadata_dir.join(dir_name);
    if !child_dir.is_dir() {
        return Vec::new();
    }

    let mut names = Vec::new();
    let entries = match fs::read_dir(&child_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        if path.extension().and_then(|ext| ext.to_str()) != Some("xml") {
            continue;
        }

        let name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .or_else(|| {
                path.strip_prefix(repo_root)
                    .ok()
                    .map(|relative| relative.to_string_lossy().to_string())
            });

        if let Some(name) = name {
            names.push(name);
        }
    }

    names.sort();
    names
}

fn render_summary_text(summary: &OneCMetadataSummary) -> String {
    let mut lines = vec![
        summary.title.clone(),
        format!("Path: {}", summary.relative_path.to_string_lossy()),
    ];

    for (label, module_path) in &summary.module_links {
        lines.push(format!("{label}: {}", module_path.to_string_lossy()));
    }

    if !summary.forms.is_empty() {
        lines.push(format!("Forms: {}", summary.forms.join(", ")));
    }

    if !summary.commands.is_empty() {
        lines.push(format!("Commands: {}", summary.commands.join(", ")));
    }

    for (key, value) in &summary.properties {
        lines.push(format!("{key}: {value}"));
    }

    lines.join("\n")
}

fn synthetic_summary_id(relative_path: &Path) -> String {
    let without_extension = relative_path.with_extension("");
    format!("onec://{}#summary", without_extension.to_string_lossy())
}

fn collect_source_paths(metadata_path: &Path, module_links: &[(String, PathBuf)]) -> Vec<PathBuf> {
    let mut paths = vec![metadata_path.to_path_buf()];
    paths.extend(module_links.iter().map(|(_, path)| path.clone()));
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_path(relative: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test-fixtures")
            .join(relative)
    }

    #[test]
    fn parses_catalog_metadata_into_summary_document() {
        let repo_root = fixture_path("onec/config-dump");
        let metadata_path = repo_root.join("Catalogs/Products/Products.xml");

        let document = OneCIngestor::new()
            .ingest_metadata_file(&repo_root, &metadata_path)
            .unwrap()
            .unwrap();

        assert_eq!(document.kind, DocumentKind::OneCMetadataSummary);
        assert_eq!(document.title, "1C Object: Catalog Products");
        assert!(document.content.contains("Manager Module: Catalogs/Products/Ext/ManagerModule.bsl"));
        assert!(document.content.contains("Object Module: Catalogs/Products/Ext/ObjectModule.bsl"));
        assert!(document.content.contains("Forms: ItemForm"));
        assert!(document.content.contains("Synonym: Products"));
        assert_eq!(document.metadata.get("object_type"), Some(&"Catalog".to_string()));
        assert_eq!(document.metadata.get("object_name"), Some(&"Products".to_string()));
    }

    #[test]
    fn parses_common_module_properties_and_module_link() {
        let repo_root = fixture_path("onec/config-dump");
        let metadata_path = repo_root.join("CommonModules/Utilities/Utilities.xml");

        let summary = OneCIngestor::new()
            .parse_metadata_summary(&repo_root, &metadata_path)
            .unwrap()
            .unwrap();

        assert_eq!(summary.object_type, "CommonModule");
        assert_eq!(summary.object_name, "Utilities");
        assert_eq!(
            summary.module_links,
            vec![(
                "Module".to_string(),
                PathBuf::from("CommonModules/Utilities/Ext/Module.bsl")
            )]
        );
        assert_eq!(summary.properties.get("Global"), Some(&"true".to_string()));
        assert_eq!(
            summary.properties.get("ClientManagedApplication"),
            Some(&"true".to_string())
        );
        assert_eq!(summary.properties.get("Server"), Some(&"true".to_string()));
    }

    #[test]
    fn parses_form_metadata_and_discovers_form_module() {
        let repo_root = fixture_path("onec/config-dump");
        let metadata_path = repo_root.join("Catalogs/Products/Forms/ItemForm.xml");

        let summary = OneCIngestor::new()
            .parse_metadata_summary(&repo_root, &metadata_path)
            .unwrap()
            .unwrap();

        assert_eq!(summary.object_type, "Form");
        assert_eq!(summary.object_name, "ItemForm");
        assert_eq!(
            summary.module_links,
            vec![(
                "Form Module".to_string(),
                PathBuf::from("Catalogs/Products/Forms/ItemForm/Ext/Form/Module.bsl")
            )]
        );
    }

    #[test]
    fn skips_non_metadata_xml_without_object_name() {
        let repo_root = fixture_path("onec/config-dump");
        let metadata_path = repo_root.join("ConfigDumpInfo.xml");

        let document = OneCIngestor::new()
            .ingest_metadata_file(&repo_root, &metadata_path)
            .unwrap();

        assert!(document.is_none());
    }

    #[test]
    fn returns_parse_error_for_malformed_xml() {
        let repo_root = fixture_path("onec/config-dump");
        let malformed = repo_root.join("Catalogs/Products/broken.xml");
        fs::write(&malformed, "<MetaDataObject><Catalog><Name>Broken</Catalog>").unwrap();

        let error = OneCIngestor::new()
            .ingest_metadata_file(&repo_root, &malformed)
            .unwrap_err();

        assert!(error.to_string().contains("Failed to parse 1C metadata XML"));

        fs::remove_file(malformed).unwrap();
    }
}
