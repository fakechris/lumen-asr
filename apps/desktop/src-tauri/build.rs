fn main() {
    generate_windows_icon();

    tauri_build::build()
}

fn generate_windows_icon() {
    use std::fs::File;
    use std::path::Path;

    let output = Path::new("icons/icon.ico");
    if output.exists() {
        return;
    }

    let mut icon = ico::IconDir::new(ico::ResourceType::Icon);
    for source in ["icons/32x32.png", "icons/128x128.png"] {
        let image = ico::IconImage::read_png(File::open(source).unwrap_or_else(|error| {
            panic!("failed to open Windows icon source {source}: {error}")
        }))
        .unwrap_or_else(|error| panic!("failed to decode Windows icon source {source}: {error}"));
        icon.add_entry(
            ico::IconDirEntry::encode(&image)
                .unwrap_or_else(|error| panic!("failed to encode {source} into ICO: {error}")),
        );
    }

    icon.write(
        File::create(output)
            .unwrap_or_else(|error| panic!("failed to create {}: {error}", output.display())),
    )
    .unwrap_or_else(|error| panic!("failed to write {}: {error}", output.display()));
}
