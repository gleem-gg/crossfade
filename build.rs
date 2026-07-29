fn main() {
    glib_build_tools::compile_resources(
        &["data"],
        "data/gleem-crossfade.gresource.xml",
        "gleem-crossfade.gresource",
    );
}
