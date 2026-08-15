//! Linker configuration for embedded-Python builds and test executables.

fn main() {
    if std::env::var_os("CARGO_FEATURE_EXTENSION_MODULE").is_some() {
        // Extension modules resolve Python symbols from the loading interpreter
        // on platforms such as macOS; they must not link or embed an rpath to
        // the build interpreter.
        pyo3_build_config::add_extension_module_link_args();
    } else {
        // Embedded-Python binaries and unit-test harnesses do link libpython.
        // Forward its runtime path from the selected interpreter configuration.
        pyo3_build_config::add_libpython_rpath_link_args();
    }
}
