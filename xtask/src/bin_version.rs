// We pin the binaries to specific versions so we use the same artifact everywhere.
// Hash of this file is used in CI for caching.

use crate::bin_install::CargoPackage;

pub const CARGO_FUZZ: CargoPackage = CargoPackage::new("cargo-fuzz", "0.12.0");
pub const CARGO_LLVM_COV: CargoPackage = CargoPackage::new("cargo-llvm-cov", "0.6.16");
pub const GRCOV: CargoPackage = CargoPackage::new("grcov", "0.8.20");
pub const WASM_PACK: CargoPackage = CargoPackage::new("wasm-pack", "0.13.1");
pub const TYPOS_CLI: CargoPackage = CargoPackage::new("typos-cli", "1.29.5").with_binary_name("typos");
pub const DIPLOMAT_TOOL: CargoPackage = CargoPackage::new("diplomat-tool", "0.7.1");

pub const WABT_VERSION: &str = "1.0.36";
pub const NIGHTLY_TOOLCHAIN: &str = "nightly-2025-07-28";
