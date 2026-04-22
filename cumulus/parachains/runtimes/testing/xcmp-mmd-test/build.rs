// This file is part of Cumulus.
// SPDX-License-Identifier: Apache-2.0

#[cfg(feature = "std")]
fn main() {
	substrate_wasm_builder::WasmBuilder::init_with_defaults()
		.build();
}

#[cfg(not(feature = "std"))]
fn main() {}
