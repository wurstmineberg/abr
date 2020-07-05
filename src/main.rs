#![deny(rust_2018_idioms, unused, unused_import_braces, unused_qualifications, warnings)]

use {
    std::{
        collections::HashMap,
        convert::{
            TryFrom as _,
            TryInto as _
        },
        io,
        path::{
            Path,
            PathBuf
        }
    },
    itertools::Itertools as _,
    j4rs::{
        ClasspathEntry,
        Instance,
        InvocationArg,
        Jvm,
        JvmBuilder,
        errors::{
            J4RsError,
            Result as JResult
        }
    },
    mcanvil::Biome,
    parking_lot::Mutex,
    rayon::prelude::*
};

const ADV_TIME_BIOMES: [Biome; 42] = [
    Biome::Badlands,
    Biome::BadlandsPlateau,
    Biome::BambooJungle,
    Biome::BambooJungleHills,
    Biome::Beach,
    Biome::BirchForest,
    Biome::BirchForestHills,
    Biome::ColdOcean,
    Biome::DarkForest,
    Biome::DeepColdOcean,
    Biome::DeepFrozenOcean,
    Biome::DeepLukewarmOcean,
    Biome::Desert,
    Biome::DesertHills,
    Biome::Forest,
    Biome::FrozenRiver,
    Biome::GiantTreeTaiga,
    Biome::GiantTreeTaigaHills,
    Biome::Jungle,
    Biome::JungleEdge,
    Biome::JungleHills,
    Biome::LukewarmOcean,
    Biome::Mountains,
    Biome::MushroomFieldShore,
    Biome::MushroomFields,
    Biome::Plains,
    Biome::River,
    Biome::Savanna,
    Biome::SavannaPlateau,
    Biome::SnowyBeach,
    Biome::SnowyMountains,
    Biome::SnowyTaiga,
    Biome::SnowyTaigaHills,
    Biome::SnowyTundra,
    Biome::StoneShore,
    Biome::Swamp,
    Biome::Taiga,
    Biome::TaigaHills,
    Biome::WarmOcean,
    Biome::WoodedBadlandsPlateau,
    Biome::WoodedHills,
    Biome::WoodedMountains
];

struct World {
    amidst: Mutex<Instance>,
    region_path: PathBuf
}

impl World {
    fn open(jvm: &Jvm, path: &Path) -> JResult<World> {
        Ok(World {
            amidst: Mutex::new(load_amidst_world(&jvm, path.to_str().ok_or_else(|| J4RsError::GeneralError(format!("path is not valid UTF-8")))?)?),
            region_path: path.join("region")
        })
    }

    fn region_uncached(&self, [region_x, region_z]: [i32; 2]) -> JResult<Option<mcanvil::Region>> {
        match mcanvil::Region::open(self.region_path.join(format!("r.{}.{}.mca", region_x, region_z))) {
            Ok(region) => Ok(Some(region)),
            Err(mcanvil::RegionDecodeError::Io(e)) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(J4RsError::GeneralError(format!("error loading file for region {}/{}: {:?}", region_x, region_z, e)))
        }
    }

    /// Returns the biome that would be found at the given block coordinates if the chunk that block column is in were to be generated now.
    fn seed_biome(&self, jvm: &Jvm, [x, z]: [i32; 2]) -> JResult<Biome> {
        let amidst_biome = jvm.invoke(
            &jvm.invoke(&self.amidst.lock(), "getBiomeDataOracle", &[])?,
            "getBiomeAt",
            &[InvocationArg::try_from(x)?.into_primitive()?, InvocationArg::try_from(z)?.into_primitive()?, InvocationArg::try_from(false)?.into_primitive()?]
        )?;
        Ok(jvm.to_rust::<String>(jvm.invoke(&amidst_biome, "getName", &[])?)?.parse().map_err(|()| J4RsError::GeneralError(format!("unknown biome name")))?)
    }

    fn region_biomes(&self, coords: [i32; 2]) -> JResult<Box<[[Option<[[Biome; 16]; 16]>; 32]; 32]>> {
        let mut buf = Box::<[[_; 32]; 32]>::default();
        if let Some(region) = self.region_uncached(coords)? {
            for chunk_col in &region {
                let chunk_col = chunk_col.map_err(|e| J4RsError::GeneralError(format!("error decoding chunk column in region {:?}: {:?}", coords, e)))?;
                let biomes_for_chunk = match chunk_col.biomes() {
                    Ok([biomes, ..]) => biomes,
                    Err(Some(-127)) => continue, // invalid biome, regenerate
                    Err(Some(bid)) => return Err(J4RsError::GeneralError(format!("unknown biome ID {} in chunk {}/{}", bid, chunk_col.level.x_pos, chunk_col.level.z_pos))),
                    Err(None) => continue // biomes not yet generated for this chunk column
                };
                buf[chunk_col.level.z_pos as usize % 32][chunk_col.level.x_pos as usize % 32] = Some(biomes_for_chunk);
            }
        }
        Ok(buf)
    }

    fn biomes_for_region(&self, jvm: &Jvm, [rx, rz]: [i32; 2], region_biomes: Box<[[Option<[[Biome; 16]; 16]>; 32]; 32]>) -> JResult<Box<[[[[Biome; 16]; 16]; 32]; 32]>> {
        let mut buf = Box::<[[_; 32]; 32]>::default();
        for (cz, chunk_row) in region_biomes.iter().enumerate() {
            for (cx, opt_chunk) in chunk_row.iter().enumerate() {
                if let Some(chunk) = opt_chunk {
                    buf[cz][cx] = *chunk;
                } else {
                    for bz in 0..16 {
                        for bx in 0..16 {
                            buf[cz][cx][bz as usize][bx as usize] = self.seed_biome(jvm, [(rz << 9) + ((cz as i32) << 4) + bx, (rx << 9) + ((cx as i32) << 4) + bz])?;
                        }
                    }
                }
            }
        }
        Ok(buf)
    }

    fn closest_adv_time_biomes(&self, jvm: &Jvm, coords: [i32; 2]) -> JResult<HashMap<Biome, [i32; 2]>> {
        let region_coords = [coords[0] >> 9, coords[1] >> 9];
        let mut found = self.closest_biomes_in_region(jvm, coords, region_coords, self.region_biomes(region_coords)?)?.into_iter().filter(|(biome, _)| ADV_TIME_BIOMES.contains(biome)).collect::<HashMap<_, _>>();
        let mut all_found = 0;
        let mut regions_scanned = 1;
        let mut total_regions = None;
        for distance in 1.. {
            let partial_biomes = coords_at_distance(region_coords, distance)
                .map(|reg| Ok((reg, self.region_biomes(reg)?)))
                .collect::<JResult<Vec<_>>>()?;
            for (reg, region_biomes) in partial_biomes {
                for (biome, [x, z]) in self.closest_biomes_in_region(jvm, coords, reg, region_biomes)? {
                    if ADV_TIME_BIOMES.contains(&biome) && taxicab_distance(coords, [x, z]) < taxicab_distance(coords, *found.entry(biome).or_insert([x, z])) {
                        found.insert(biome, [x, z]);
                    }
                }
                regions_scanned += 1;
                if let Some(total) = total_regions {
                    eprint!("\rall {} biomes found, scanning 2 more rings to ensure minimal distance: {}/{} regions", ADV_TIME_BIOMES.len(), regions_scanned, total);
                } else {
                    eprint!("\r{} regions scanned, {}/{} biomes found", regions_scanned, found.len(), ADV_TIME_BIOMES.len());
                }
            }
            if all_found >= 2 {
                break
            } else if found.len() >= ADV_TIME_BIOMES.len() {
                all_found += 1; // run 2 more rings to make sure distances are minimal
                if total_regions.is_none() {
                    total_regions = Some(regions_scanned + coords_at_distance(region_coords, distance + 1).count() + coords_at_distance(region_coords, distance + 2).count());
                    eprintln!();
                }
            }
        }
        eprintln!();
        for (biome, &[x, z]) in found.iter().sorted_by_key(|(biome, &[x, z])| (taxicab_distance(coords, [x, z]), z, x, biome.to_string())) {
            let biome_dist = taxicab_distance(coords, [x, z]);
            eprintln!("closest {} at {}/{} (distance: {}m)", biome, x, z, biome_dist);
        }
        Ok(found)
    }

    fn closest_biomes_in_region(&self, jvm: &Jvm, coords: [i32; 2], [rx, rz]: [i32; 2], region_biomes: Box<[[Option<[[Biome; 16]; 16]>; 32]; 32]>) -> JResult<HashMap<Biome, [i32; 2]>> {
        let mut found = HashMap::default();
        for (cz, chunk_row) in self.biomes_for_region(jvm, [rx, rz], region_biomes)?.iter().enumerate() {
            for (cx, chunk) in chunk_row.iter().enumerate() {
                for (bz, block_row) in chunk.iter().enumerate() {
                    for (bx, &biome) in block_row.iter().enumerate() {
                        let block_coords = [(rx << 9) + ((cx as i32) << 4) + bx as i32, (rz << 9) + ((cz as i32) << 4) + bz as i32];
                        if taxicab_distance(coords, block_coords) < taxicab_distance(coords, *found.entry(biome).or_insert(block_coords)) {
                            found.insert(biome, block_coords);
                        }
                    }
                }
            }
        }
        Ok(found)
    }
}

fn load_amidst_world(jvm: &Jvm, path: &str) -> JResult<Instance> { //TODO use `path: impl AsRef<std::path::Path>`
    // from src/main/java/amidst/PerApplicationInjector.java line 62
    // new PlayerInformationCache()
    let player_information_provider = jvm.cast(
        &jvm.create_instance(
            "amidst.mojangapi.file.PlayerInformationCache",
            &[]
        )?,
        "amidst.mojangapi.file.PlayerInformationProvider"
    )?;
    // from src/main/java/amidst/PerApplicationInjector.java line 63
    // SeedHistoryLogger.from(parameters.seedHistoryFile)
    let seed_history_logger = jvm.invoke_static(
        "amidst.mojangapi.world.SeedHistoryLogger",
        "createDisabled",
        &[]
    )?;
    // from src/main/java/amidst/PerApplicationInjector.java line 67
    // new WorldBuilder(playerInformationProvider, seedHistoryLogger)
    let world_builder = jvm.create_instance(
        "amidst.mojangapi.world.WorldBuilder",
        &[player_information_provider.into(), seed_history_logger.into()]
    )?;
    // from src/main/java/amidst/PerApplicationInjector.java line 71
    // VersionListProvider.createLocalAndStartDownloadingRemote(threadMaster.getWorkerExecutor())
    let local_version_list = jvm.invoke_static(
        "amidst.mojangapi.file.VersionList",
        "newLocalVersionList",
        &[]
    )?;
    let version_list_provider = jvm.create_instance(
        "amidst.mojangapi.file.VersionListProvider",
        &[local_version_list.into()]
    )?;
    // from src/main/java/amidst/PerApplicationInjector.java line 64
    // MinecraftInstallation.newLocalMinecraftInstallation(parameters.dotMinecraftDirectory)
    let minecraft_installation = jvm.invoke_static(
        "amidst.mojangapi.file.MinecraftInstallation",
        "newLocalMinecraftInstallation",
        &[]
    )?;
    // from src/main/java/amidst/gui/profileselect/ProfileSelectWindow line 128
    let launcher_profiles = jvm.invoke(
        &minecraft_installation,
        "readLauncherProfiles",
        &[]
    )?;
    let mut unresolved_profile = None;
    for i in 0..jvm.to_rust(jvm.invoke(&launcher_profiles, "size", &[])?)? {
        let profile = jvm.cast(
            &jvm.invoke(
                &launcher_profiles,
                "get",
                &[InvocationArg::try_from(i)?.into_primitive()?]
            )?,
            "amidst.mojangapi.file.UnresolvedLauncherProfile"
        )?;
        if jvm.to_rust::<String>(jvm.invoke(&profile, "getName", &[])?)? == "Wurstmineberg" {
            unresolved_profile = Some(profile);
            break;
        }
    }
    let unresolved_profile = unresolved_profile.ok_or_else(|| J4RsError::GeneralError(format!("wurstmineberg profile not found")))?;
    // from src/main/java/amidst/gui/profileselect/LocalProfileComponent.java line 97
    // unresolvedProfile.resolveToVanilla(versionListProvider.getRemoteOrElseLocal())
    let version_list = jvm.invoke(
        &version_list_provider,
        "getRemoteOrElseLocal",
        &[]
    )?;
    let launcher_profile = jvm.invoke(
        &unresolved_profile,
        "resolveToVanilla",
        &[version_list.into()]
    )?;
    // from src/main/java/amidst/mojangapi/LauncherProfileRunner.java line 22
    // RunningLauncherProfile.from(worldBuilder, launcherProfile, initialWorldOptions)
    let initial_world_options = jvm.invoke_static(
        "java.util.Optional",
        "empty",
        &[]
    )?;
    let running_launcher_profile = jvm.invoke_static(
        "amidst.mojangapi.RunningLauncherProfile",
        "from",
        &[world_builder.into(), launcher_profile.into(), initial_world_options.into()]
    )?;
    // from src/main/java/amidst/gui/main/WorldSwitcher.java line 78
    // runningLauncherProfile.createWorldFromSaveGame(minecraftInstallation.newSaveGame(path))
    let path = jvm.invoke_static(
        "java.nio.file.Paths",
        "get",
        &[path.try_into()?, jvm.create_java_array("java.lang.String", &[])?.try_into()?]
    )?;
    let save_game = jvm.invoke(
        &minecraft_installation,
        "newSaveGame",
        &[path.into()]
    )?;
    jvm.invoke(
        &running_launcher_profile,
        "createWorldFromSaveGame",
        &[save_game.into()]
    )
}

/// Yields all coordinates with the given horizontal taxicab distance from the given center.
///
/// Distance must be at least 1.
fn coords_at_distance([x, z]: [i32; 2], distance: i32) -> impl ParallelIterator<Item = [i32; 2]> {
    (0..distance).into_par_iter().map(move |d| [x + d, z - distance + d])
        .chain((0..distance).into_par_iter().map(move |d| [x + distance - d, z + d]))
        .chain((0..distance).into_par_iter().map(move |d| [x - d, z + distance - d]))
        .chain((0..distance).into_par_iter().map(move |d| [x - distance + d, z - d]))
}

fn taxicab_distance([x1, z1]: [i32; 2], [x2, z2]: [i32; 2]) -> u32 {
    (x2 - x1).abs() as u32 + (z2 - z1).abs() as u32
}

fn main() -> JResult<()> {
    let path = Path::new("C:\\Users\\Fenhl\\games\\minecraft\\srv\\wmb\\backup\\wmb-world_2020-07-03_21-57-12_1.16.1"); //TODO add command-line option to change the path?
    let jvm = JvmBuilder::new().classpath_entry(ClasspathEntry::new("C:\\Users\\Fenhl\\games\\minecraft\\amidst-v4-5-beta3.jar")).build()?; //TODO auto-download appropriate Amidst release?
    let world = World::open(&jvm, path)?;
    let _ = world.closest_adv_time_biomes(&jvm, [3386, 3096])?; //TODO suggest a path for the railway
    Ok(())
}
