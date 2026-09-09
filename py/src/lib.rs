//! `pybo` — the Python face of `bo`, binding only the public `bo::client`
//! surface (the [`Bo`] struct and the vocabulary types its methods speak),
//! never the raw `Command` protocol or the engine.
//!
//! Every verb travels to the daemon over its Unix socket, exactly like the
//! Rust client does; the daemon binary is found by `bo` itself (PATH, or a
//! `target/{debug,release}/bo` in this checkout), so a Python host needs no
//! extra setup.

use pyo3::prelude::*;

/// The package version.
#[pyfunction]
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[pymodule]
fn pybo(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(version, m)?)?;
    Ok(())
}
