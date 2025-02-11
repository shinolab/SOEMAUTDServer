use std::path::Path;

fn main() -> anyhow::Result<()> {
    let license_file_map = Vec::new();

    if autd3_license_check::check(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml"),
        "ThirdPartyNotice",
        &license_file_map,
        &[("SOEM", "SOEM\nhttps://github.com/OpenEtherCATsociety/SOEM")],
    )? {
        return Err(anyhow::anyhow!(
            "Some ThirdPartyNotice.txt files have been updated. Manuall check is required.",
        ));
    }

    Ok(())
}
