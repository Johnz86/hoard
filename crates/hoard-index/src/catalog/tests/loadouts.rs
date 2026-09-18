use hoard_core::LoadoutId;

use super::*;

#[test]
fn a_loadout_is_created_under_a_free_name_and_renamed_in_place() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    let night = catalog.create_loadout(&Loadout::new("night"))?;
    assert!(matches!(
        catalog.create_loadout(&Loadout::new("night")),
        Err(IndexError::Domain(_))
    ));
    assert_eq!(catalog.list_loadouts()?.len(), 1);

    let day = catalog.create_loadout(&Loadout::new("day"))?;
    assert_ne!(day, night);
    assert!(matches!(
        catalog.rename_loadout(day, "night"),
        Err(IndexError::Domain(_))
    ));
    assert_eq!(
        catalog.loadout(day)?.map(|record| record.name),
        Some("day".to_owned())
    );

    let renamed = catalog.rename_loadout(day, "dusk")?;
    assert_eq!(renamed.name, "dusk");
    assert_eq!(renamed.parse()?.name, "dusk");
    assert!(catalog.loadout_by_name("day")?.is_none());
    assert!(catalog.loadout_by_name("dusk")?.is_some());
    assert_eq!(catalog.rename_loadout(day, "dusk")?.name, "dusk");

    assert!(matches!(
        catalog.rename_loadout(LoadoutId::new(999), "x"),
        Err(IndexError::NotFound { .. })
    ));
    assert!(catalog.delete_loadout(day)?);
    assert_eq!(catalog.list_loadouts()?.len(), 1);
    assert!(catalog.loadout(night)?.is_some());
    Ok(())
}
