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
    Synthetic {
        synthetic_path: String,
        primary_source: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DocumentKind {
    OneCMetadataSummary,
    OneCObjectBundle,
    OneCFormModuleBundle,
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
        let mut documents = self.ingest_metadata_documents(repo_root, metadata_path)?;
        let summary = documents
            .drain(..)
            .find(|document| document.kind == DocumentKind::OneCMetadataSummary);
        Ok(summary)
    }

    pub fn ingest_metadata_documents(
        &self,
        repo_root: &Path,
        metadata_path: &Path,
    ) -> Result<Vec<NormalizedDocument>> {
        let summary = match self.parse_metadata_summary(repo_root, metadata_path)? {
            Some(summary) => summary,
            None => return Ok(Vec::new()),
        };

        let mut metadata = summary.properties.clone();
        metadata.insert("object_type".to_string(), summary.object_type.clone());
        metadata.insert("object_name".to_string(), summary.object_name.clone());
        metadata.insert(
            "relative_path".to_string(),
            summary.relative_path.to_string_lossy().to_string(),
        );
        metadata.insert(
            "synthetic_id".to_string(),
            synthetic_summary_id(&summary.relative_path),
        );
        metadata.insert(
            "primary_source_path".to_string(),
            summary.relative_path.to_string_lossy().to_string(),
        );

        let mut documents = vec![NormalizedDocument {
            id: synthetic_summary_id(&summary.relative_path),
            title: summary.title.clone(),
            kind: DocumentKind::OneCMetadataSummary,
            origin: DocumentOrigin::File(metadata_path.to_path_buf()),
            source_paths: collect_source_paths(&summary.relative_path, &summary.module_links),
            language: "text".to_string(),
            content: render_summary_text(&summary),
            metadata,
        }];

        if let Some(bundle) = self.build_linked_document(repo_root, metadata_path, &summary)? {
            documents.push(bundle);
        }

        Ok(documents)
    }

    pub fn parse_metadata_summary(
        &self,
        repo_root: &Path,
        metadata_path: &Path,
    ) -> Result<Option<OneCMetadataSummary>> {
        let xml = fs::read_to_string(metadata_path).with_context(|| {
            format!(
                "Failed to read 1C metadata file {}",
                metadata_path.display()
            )
        })?;
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

    fn build_linked_document(
        &self,
        repo_root: &Path,
        metadata_path: &Path,
        summary: &OneCMetadataSummary,
    ) -> Result<Option<NormalizedDocument>> {
        let linked_forms = discover_linked_forms(self, repo_root, metadata_path);

        if summary.module_links.is_empty() && linked_forms.is_empty() {
            return Ok(None);
        }

        let is_form = summary.object_type == "Form";
        let kind = if is_form {
            DocumentKind::OneCFormModuleBundle
        } else {
            DocumentKind::OneCObjectBundle
        };
        let title = if is_form {
            format!("1C Form Module Bundle: {}", summary.object_name)
        } else {
            format!(
                "1C Object Bundle: {} {}",
                summary.object_type, summary.object_name
            )
        };

        let mut metadata = summary.properties.clone();
        metadata.insert("object_type".to_string(), summary.object_type.clone());
        metadata.insert("object_name".to_string(), summary.object_name.clone());
        metadata.insert(
            "relative_path".to_string(),
            summary.relative_path.to_string_lossy().to_string(),
        );
        metadata.insert(
            "summary_document_id".to_string(),
            synthetic_summary_id(&summary.relative_path),
        );
        metadata.insert(
            "primary_source_path".to_string(),
            summary.relative_path.to_string_lossy().to_string(),
        );
        metadata.insert(
            "linked_module_count".to_string(),
            summary.module_links.len().to_string(),
        );
        metadata.insert(
            "linked_form_count".to_string(),
            linked_forms.len().to_string(),
        );
        if !summary.forms.is_empty() {
            metadata.insert("forms".to_string(), summary.forms.join(", "));
        }
        if !summary.module_links.is_empty() {
            metadata.insert(
                "linked_modules".to_string(),
                summary
                    .module_links
                    .iter()
                    .map(|(label, path)| format!("{label}: {}", path.to_string_lossy()))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }

        let mut source_paths = collect_source_paths(&summary.relative_path, &summary.module_links);
        for linked_form in &linked_forms {
            if !source_paths.contains(&linked_form.relative_path) {
                source_paths.push(linked_form.relative_path.clone());
            }
            for (_, module_path) in &linked_form.module_links {
                if !source_paths.contains(module_path) {
                    source_paths.push(module_path.clone());
                }
            }
        }

        Ok(Some(NormalizedDocument {
            id: synthetic_bundle_id(&summary.relative_path, is_form),
            title: title.clone(),
            kind,
            origin: DocumentOrigin::Synthetic {
                synthetic_path: synthetic_bundle_id(&summary.relative_path, is_form),
                primary_source: summary.relative_path.clone(),
            },
            source_paths,
            language: "text".to_string(),
            content: render_linked_document_text(repo_root, &title, summary, &linked_forms),
            metadata,
        }))
    }
}

#[derive(Debug, Clone)]
struct LinkedForm {
    relative_path: PathBuf,
    object_name: String,
    module_links: Vec<(String, PathBuf)>,
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
        (
            "Manager Module",
            metadata_dir.join("Ext").join("ManagerModule.bsl"),
        ),
        (
            "Object Module",
            metadata_dir.join("Ext").join("ObjectModule.bsl"),
        ),
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
    format!("onec://{}#summary", synthetic_base_path(relative_path))
}

fn synthetic_bundle_id(relative_path: &Path, is_form: bool) -> String {
    let suffix = if is_form { "form-module" } else { "bundle" };
    format!("onec://{}#{suffix}", synthetic_base_path(relative_path))
}

fn synthetic_base_path(relative_path: &Path) -> String {
    let without_extension = relative_path.with_extension("");

    if is_primary_object_metadata_path(relative_path) {
        return without_extension
            .parent()
            .unwrap_or(&without_extension)
            .to_string_lossy()
            .to_string();
    }

    without_extension.to_string_lossy().to_string()
}

fn is_primary_object_metadata_path(relative_path: &Path) -> bool {
    let Some(stem) = relative_path.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    let Some(parent_name) = relative_path.parent().and_then(|parent| parent.file_name()) else {
        return false;
    };

    parent_name == stem
        && relative_path.parent().and_then(Path::file_name) != Some("Forms".as_ref())
}

fn collect_source_paths(metadata_path: &Path, module_links: &[(String, PathBuf)]) -> Vec<PathBuf> {
    let mut paths = vec![metadata_path.to_path_buf()];
    paths.extend(module_links.iter().map(|(_, path)| path.clone()));
    paths
}

fn discover_linked_forms(
    ingestor: &OneCIngestor,
    repo_root: &Path,
    metadata_path: &Path,
) -> Vec<LinkedForm> {
    let Some(metadata_dir) = metadata_path.parent() else {
        return Vec::new();
    };

    let forms_dir = metadata_dir.join("Forms");
    if !forms_dir.is_dir() {
        return Vec::new();
    }

    let entries = match fs::read_dir(&forms_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut forms = Vec::new();
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("xml") {
            continue;
        }

        let Ok(Some(summary)) = ingestor.parse_metadata_summary(repo_root, &path) else {
            continue;
        };

        forms.push(LinkedForm {
            relative_path: summary.relative_path,
            object_name: summary.object_name,
            module_links: summary.module_links,
        });
    }

    forms.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    forms
}

fn render_linked_document_text(
    repo_root: &Path,
    title: &str,
    summary: &OneCMetadataSummary,
    linked_forms: &[LinkedForm],
) -> String {
    let mut lines = vec![
        title.to_string(),
        format!("Metadata Path: {}", summary.relative_path.to_string_lossy()),
        format!(
            "Summary Document: {}",
            synthetic_summary_id(&summary.relative_path)
        ),
    ];

    if !summary.forms.is_empty() {
        lines.push(format!("Forms: {}", summary.forms.join(", ")));
    }

    if !summary.commands.is_empty() {
        lines.push(format!("Commands: {}", summary.commands.join(", ")));
    }

    for (label, module_path) in &summary.module_links {
        lines.push(format!("{label}: {}", module_path.to_string_lossy()));
        if let Some(module_source) = read_repo_file(repo_root, module_path) {
            lines.push(String::new());
            lines.push(format!("[{label} Source]"));
            lines.push(module_source);
        }
    }

    for linked_form in linked_forms {
        lines.push(String::new());
        lines.push(format!(
            "Form Metadata: {}",
            linked_form.relative_path.to_string_lossy()
        ));
        lines.push(format!("Form Name: {}", linked_form.object_name));

        for (label, module_path) in &linked_form.module_links {
            lines.push(format!("{label}: {}", module_path.to_string_lossy()));
            if let Some(module_source) = read_repo_file(repo_root, module_path) {
                lines.push(String::new());
                lines.push(format!("[{} Source: {}]", label, linked_form.object_name));
                lines.push(module_source);
            }
        }
    }

    lines.join("\n")
}

fn read_repo_file(repo_root: &Path, relative_path: &Path) -> Option<String> {
    fs::read_to_string(repo_root.join(relative_path))
        .ok()
        .map(|content| content.trim().to_string())
        .filter(|content| !content.is_empty())
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
        assert!(document
            .content
            .contains("Manager Module: Catalogs/Products/Ext/ManagerModule.bsl"));
        assert!(document
            .content
            .contains("Object Module: Catalogs/Products/Ext/ObjectModule.bsl"));
        assert!(document.content.contains("Forms: ItemForm"));
        assert!(document.content.contains("Synonym: Products"));
        assert_eq!(document.id, "onec://Catalogs/Products#summary");
        assert_eq!(
            document.metadata.get("object_type"),
            Some(&"Catalog".to_string())
        );
        assert_eq!(
            document.metadata.get("object_name"),
            Some(&"Products".to_string())
        );
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
    fn emits_linked_catalog_bundle_with_modules_and_forms() {
        let repo_root = fixture_path("onec/config-dump");
        let metadata_path = repo_root.join("Catalogs/Products/Products.xml");

        let documents = OneCIngestor::new()
            .ingest_metadata_documents(&repo_root, &metadata_path)
            .unwrap();

        assert_eq!(documents.len(), 2);

        let bundle = documents
            .iter()
            .find(|document| document.kind == DocumentKind::OneCObjectBundle)
            .unwrap();

        assert_eq!(bundle.id, "onec://Catalogs/Products#bundle");
        assert_eq!(bundle.title, "1C Object Bundle: Catalog Products");
        assert_eq!(
            bundle.origin,
            DocumentOrigin::Synthetic {
                synthetic_path: "onec://Catalogs/Products#bundle".to_string(),
                primary_source: PathBuf::from("Catalogs/Products/Products.xml"),
            }
        );
        assert!(bundle
            .source_paths
            .contains(&PathBuf::from("Catalogs/Products/Products.xml")));
        assert!(bundle
            .source_paths
            .contains(&PathBuf::from("Catalogs/Products/Ext/ManagerModule.bsl")));
        assert!(bundle
            .source_paths
            .contains(&PathBuf::from("Catalogs/Products/Ext/ObjectModule.bsl")));
        assert!(bundle
            .source_paths
            .contains(&PathBuf::from("Catalogs/Products/Forms/ItemForm.xml")));
        assert!(bundle
            .content
            .contains("Summary Document: onec://Catalogs/Products#summary"));
        assert!(bundle.content.contains("[Manager Module Source]"));
        assert!(bundle
            .content
            .contains("Procedure BeforeWrite(Cancel, WriteMode)"));
        assert!(bundle
            .content
            .contains("Procedure OnCreateAtServer(Cancel, StandardProcessing)"));
        assert!(bundle
            .content
            .contains("Form Metadata: Catalogs/Products/Forms/ItemForm.xml"));
        assert!(bundle.content.contains("[Form Module Source: ItemForm]"));
        assert!(bundle.content.contains("Procedure OnOpen()"));
    }

    #[test]
    fn emits_form_module_bundle_for_form_metadata() {
        let repo_root = fixture_path("onec/config-dump");
        let metadata_path = repo_root.join("Catalogs/Products/Forms/ItemForm.xml");

        let documents = OneCIngestor::new()
            .ingest_metadata_documents(&repo_root, &metadata_path)
            .unwrap();

        assert_eq!(documents.len(), 2);

        let bundle = documents
            .iter()
            .find(|document| document.kind == DocumentKind::OneCFormModuleBundle)
            .unwrap();

        assert_eq!(
            bundle.id,
            "onec://Catalogs/Products/Forms/ItemForm#form-module"
        );
        assert_eq!(bundle.title, "1C Form Module Bundle: ItemForm");
        assert!(bundle
            .content
            .contains("Metadata Path: Catalogs/Products/Forms/ItemForm.xml"));
        assert!(bundle
            .content
            .contains("Summary Document: onec://Catalogs/Products/Forms/ItemForm#summary"));
        assert!(bundle
            .content
            .contains("Form Module: Catalogs/Products/Forms/ItemForm/Ext/Form/Module.bsl"));
        assert!(bundle.content.contains("Procedure OnOpen()"));
    }

    #[test]
    fn emits_common_module_bundle_with_module_source() {
        let repo_root = fixture_path("onec/config-dump");
        let metadata_path = repo_root.join("CommonModules/Utilities/Utilities.xml");

        let documents = OneCIngestor::new()
            .ingest_metadata_documents(&repo_root, &metadata_path)
            .unwrap();

        assert_eq!(documents.len(), 2);

        let bundle = documents
            .iter()
            .find(|document| document.kind == DocumentKind::OneCObjectBundle)
            .unwrap();

        assert_eq!(bundle.id, "onec://CommonModules/Utilities#bundle");
        assert!(bundle
            .content
            .contains("Module: CommonModules/Utilities/Ext/Module.bsl"));
        assert!(bundle.content.contains("Function FormatItemCode(Code)"));
        assert!(bundle
            .content
            .contains("Procedure NotifyUser(Message) Export"));
        assert_eq!(
            bundle.metadata.get("linked_module_count"),
            Some(&"1".to_string())
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
        fs::write(
            &malformed,
            "<MetaDataObject><Catalog><Name>Broken</Catalog>",
        )
        .unwrap();

        let error = OneCIngestor::new()
            .ingest_metadata_file(&repo_root, &malformed)
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("Failed to parse 1C metadata XML"));

        fs::remove_file(malformed).unwrap();
    }
}
