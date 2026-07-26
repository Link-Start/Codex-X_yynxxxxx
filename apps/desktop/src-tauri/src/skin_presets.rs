pub(crate) struct BuiltinSkinAsset {
    pub(crate) id: &'static str,
    pub(crate) manifest: &'static str,
    pub(crate) image: &'static [u8],
}

pub(crate) const BUILTIN_SKIN_ID: &str = "shiina-mashiro-blossom";

pub(crate) fn retired_builtin_skin_ids() -> &'static [&'static str] {
    &[
        "preset-midnight-aurora",
        "preset-sakura-dawn",
        "preset-amber-dusk",
        "preset-forest-mist",
        "preset-cyber-neon",
    ]
}

pub(crate) fn builtin_skin_assets() -> &'static [BuiltinSkinAsset] {
    &[]
}
