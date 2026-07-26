use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_theme_dir(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "codex-x-skin-{name}-{}-{}",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("create theme fixture");
    path
}

fn write_theme_fixture(dir: &Path, manifest: &str) {
    fs::write(dir.join("theme.json"), manifest).expect("write manifest");
    fs::write(dir.join("background.jpg"), [0xff, 0xd8, 0xff, 0xd9]).expect("write image");
}

#[test]
fn manifest_accepts_adaptive_fields_and_preserves_extensions() {
    let dir = temp_theme_dir("adaptive");
    write_theme_fixture(
        &dir,
        r##"{
          "schemaVersion": 1,
          "id": "adaptive",
          "name": "Adaptive",
          "image": "background.jpg",
          "appearance": "auto",
          "art": { "focusX": 0.72, "safeArea": "left", "taskMode": "ambient" },
          "promoTitle": "kept"
        }"##,
    );

    let manifest = read_manifest(&dir).expect("parse adaptive manifest");

    assert!(manifest.colors.is_none());
    assert_eq!(
        manifest.art.as_ref().and_then(|art| art.focus_x),
        Some(0.72)
    );
    assert_eq!(
        manifest.extra.get("promoTitle"),
        Some(&Value::String("kept".to_string()))
    );
    let serialized = serde_json::to_value(manifest).expect("serialize manifest");
    assert_eq!(serialized["promoTitle"], "kept");
    fs::remove_dir_all(dir).expect("remove theme fixture");
}

#[test]
fn manifest_rejects_out_of_range_art_focus() {
    let dir = temp_theme_dir("focus");
    write_theme_fixture(
        &dir,
        r#"{
          "schemaVersion": 1,
          "id": "bad-focus",
          "name": "Bad focus",
          "image": "background.jpg",
          "art": { "focusX": 2 }
        }"#,
    );

    let error = read_manifest(&dir).expect_err("reject invalid focus");

    assert!(error.to_string().contains("focusX"));
    fs::remove_dir_all(dir).expect("remove theme fixture");
}

#[cfg(unix)]
#[test]
fn manifest_rejects_symlinked_images() {
    use std::os::unix::fs::symlink;

    let dir = temp_theme_dir("symlink");
    let outside = dir.with_extension("outside.jpg");
    fs::write(&outside, [0xff, 0xd8, 0xff, 0xd9]).expect("write outside image");
    fs::write(
        dir.join("theme.json"),
        r#"{"schemaVersion":1,"id":"linked","name":"Linked","image":"background.jpg"}"#,
    )
    .expect("write manifest");
    symlink(&outside, dir.join("background.jpg")).expect("create image symlink");

    let error = read_manifest(&dir).expect_err("reject image symlink");

    assert!(error.to_string().contains("普通文件"));
    fs::remove_dir_all(dir).expect("remove theme fixture");
    fs::remove_file(outside).expect("remove outside image");
}

#[test]
fn zip_import_rejects_parent_traversal() {
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    archive
        .start_file("../theme.json", SimpleFileOptions::default())
        .expect("start unsafe entry");
    archive.write_all(b"{}").expect("write unsafe entry");
    let bytes = archive.finish().expect("finish archive").into_inner();

    let error = import_skin_theme_zip_inner("unsafe.zip".to_string(), bytes)
        .expect_err("reject traversal archive");

    assert!(error.to_string().contains("越界路径"));
}

#[test]
fn theme_id_length_is_bounded() {
    let error =
        normalize_theme_id(&"a".repeat(MAX_THEME_ID_BYTES + 1)).expect_err("reject long theme id");
    assert!(error.to_string().contains("主题 ID"));
}

#[test]
fn uploaded_image_requires_matching_supported_signature() {
    assert_eq!(
        uploaded_image_extension(
            "wallpaper.png",
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
        )
        .expect("accept PNG signature"),
        "png"
    );
    assert_eq!(
        uploaded_image_extension("wallpaper.jpeg", &[0xff, 0xd8, 0xff, 0xd9])
            .expect("accept JPEG signature"),
        "jpg"
    );
    assert!(uploaded_image_extension("wallpaper.png", b"not an image")
        .expect_err("reject disguised image")
        .to_string()
        .contains("格式不匹配"));
}

#[test]
fn image_theme_names_and_ids_are_safe_and_unique() {
    assert_eq!(uploaded_theme_name("  sakura night.webp  "), "sakura night");
    let root = temp_theme_dir("unique-image-id");
    fs::create_dir_all(root.join("image-sakura-night")).expect("create existing theme");
    assert_eq!(
        unique_theme_id_in(&root, "image-sakura-night").expect("create unique id"),
        "image-sakura-night-2"
    );
    fs::remove_dir_all(root).expect("remove unique id fixture");
}

#[test]
fn image_upload_creates_an_adaptive_theme_without_overwriting() {
    let file_name = format!(
        "direct-image-test-{}.png",
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let result = create_skin_theme_from_image_inner(file_name, bytes)
        .expect("create adaptive theme from image");
    let theme = result
        .state
        .themes
        .iter()
        .find(|theme| theme.name.starts_with("direct-image-test-"))
        .expect("created theme is listed");
    let theme_dir = themes_root().expect("theme root").join(&theme.id);
    let manifest = read_manifest(&theme_dir).expect("read created theme");

    assert_eq!(manifest.appearance.as_deref(), Some("auto"));
    assert_eq!(manifest.surface_opacity, Some(IMAGE_THEME_SURFACE_OPACITY));
    assert!(manifest.colors.is_none());
    assert!(theme.adaptive);
    assert_eq!(theme.surface_opacity, IMAGE_THEME_SURFACE_OPACITY);
    assert_eq!(
        manifest
            .art
            .as_ref()
            .and_then(|art| art.safe_area.as_deref()),
        Some("auto")
    );
    fs::remove_dir_all(theme_dir).expect("remove created image theme");
}

#[test]
fn imported_theme_metadata_can_be_edited_without_changing_theme_assets() {
    let id = format!(
        "metadata-edit-test-{}",
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let theme_dir = themes_root().expect("theme root").join(&id);
    fs::create_dir_all(&theme_dir).expect("create editable theme");
    fs::write(theme_dir.join("background.jpg"), [0xff, 0xd8, 0xff, 0xd9])
        .expect("write editable theme image");
    let manifest = SkinThemeManifest {
        schema_version: 1,
        id: id.clone(),
        name: "Before".to_string(),
        brand_subtitle: "Keep brand".to_string(),
        tagline: "Before tagline".to_string(),
        project_prefix: String::new(),
        project_label: String::new(),
        status_text: String::new(),
        quote: "Keep quote".to_string(),
        image: "background.jpg".to_string(),
        appearance: Some("auto".to_string()),
        surface_opacity: None,
        art: Some(SkinThemeArt {
            safe_area: Some("left".to_string()),
            ..SkinThemeArt::default()
        }),
        colors: None,
        extra: BTreeMap::from([("customField".to_string(), Value::Bool(true))]),
    };
    write_manifest(&theme_dir, &manifest).expect("write editable manifest");

    let result = update_skin_theme_settings_inner(
        id.clone(),
        "  After  ".to_string(),
        "  Updated tagline  ".to_string(),
        0.55,
    )
    .expect("update metadata");
    let updated = read_manifest(&theme_dir).expect("read updated manifest");

    assert_eq!(updated.name, "After");
    assert_eq!(updated.tagline, "Updated tagline");
    assert_eq!(updated.brand_subtitle, "Keep brand");
    assert_eq!(updated.quote, "Keep quote");
    assert_eq!(updated.image, "background.jpg");
    assert_eq!(updated.surface_opacity, Some(0.55));
    assert_eq!(
        updated.art.and_then(|art| art.safe_area),
        Some("left".to_string())
    );
    assert_eq!(updated.extra.get("customField"), Some(&Value::Bool(true)));
    assert_eq!(
        result
            .state
            .themes
            .iter()
            .find(|theme| theme.id == id)
            .map(|theme| theme.name.as_str()),
        Some("After")
    );
    fs::remove_dir_all(theme_dir).expect("remove editable theme");
}

#[test]
fn builtin_catalog_is_not_embedded_but_source_remains_valid() {
    let assets = builtin_skin_assets();
    assert!(assets.is_empty());
    let manifest: SkinThemeManifest = serde_json::from_str(include_str!(
        "../resources/skin-presets/shiina-mashiro-blossom/theme.json"
    ))
    .expect("parse retained Shiina theme source");
    assert_eq!(manifest.id, BUILTIN_SKIN_ID);
    assert_eq!(manifest.name, "椎名真白·樱花画室");
    assert_eq!(manifest.quote, "安静创作，也会发光");
    assert!(retired_builtin_skin_ids()
        .iter()
        .all(|retired| *retired != manifest.id));
}

#[test]
fn export_destination_adds_zip_extension_and_rejects_other_types() {
    let dir = temp_theme_dir("export-destination");
    let without_extension = dir.join("my-theme");
    let normalized = normalize_export_destination(without_extension.to_string_lossy().as_ref())
        .expect("append zip extension");
    assert_eq!(normalized, dir.join("my-theme.zip"));

    let error = normalize_export_destination(dir.join("my-theme.txt").to_string_lossy().as_ref())
        .expect_err("reject non-zip extension");
    assert!(error.to_string().contains(".zip"));
    fs::remove_dir_all(dir).expect("remove export fixture");
}
