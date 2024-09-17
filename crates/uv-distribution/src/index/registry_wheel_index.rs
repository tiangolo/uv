use indexmap::IndexMap;
use rustc_hash::FxHashMap;
use std::collections::hash_map::Entry;
use std::collections::BTreeMap;

use distribution_types::{CachedRegistryDist, Hashed, Index, IndexLocations, IndexUrl};
use pep440_rs::Version;
use platform_tags::Tags;
use uv_cache::{Cache, CacheBucket, WheelCache};
use uv_fs::{directories, files, symlinks};
use uv_normalize::PackageName;
use uv_types::HashStrategy;

use crate::index::cached_wheel::CachedWheel;
use crate::source::{HttpRevisionPointer, LocalRevisionPointer, HTTP_REVISION, LOCAL_REVISION};

/// A local index of distributions that originate from a registry, like `PyPI`.
#[derive(Debug)]
pub struct RegistryWheelIndex<'a> {
    cache: &'a Cache,
    tags: &'a Tags,
    index_locations: &'a IndexLocations,
    hasher: &'a HashStrategy,
    /// The cached distributions, indexed by package name and index.
    ///
    /// Index priority is respected, such that if a version is found in multiple indexes, the
    /// highest priority index is
    index: FxHashMap<&'a PackageName, IndexMap<Index, BTreeMap<Version, CachedRegistryDist>>>,
}

impl<'a> RegistryWheelIndex<'a> {
    /// Initialize an index of registry distributions.
    pub fn new(
        cache: &'a Cache,
        tags: &'a Tags,
        index_locations: &'a IndexLocations,
        hasher: &'a HashStrategy,
    ) -> Self {
        Self {
            cache,
            tags,
            index_locations,
            hasher,
            index: FxHashMap::default(),
        }
    }

    /// Return an iterator over available wheels for a given package.
    ///
    /// If the package is not yet indexed, this will index the package by reading from the cache.
    pub fn get(
        &mut self,
        name: &'a PackageName,
    ) -> impl Iterator<Item = (&Index, &Version, &CachedRegistryDist)> {
        self.get_impl(name).iter().flat_map(|(index, versions)| {
            versions
                .iter()
                .map(move |(version, dist)| (index, version, dist))
        })
    }

    /// Get an entry in the index.
    fn get_impl(
        &mut self,
        name: &'a PackageName,
    ) -> &IndexMap<Index, BTreeMap<Version, CachedRegistryDist>> {
        let by_index = match self.index.entry(name) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(Self::index(
                name,
                self.cache,
                self.tags,
                self.index_locations,
                self.hasher,
            )),
        };
        by_index
    }

    /// Add a package to the index by reading from the cache.
    fn index(
        package: &PackageName,
        cache: &Cache,
        tags: &Tags,
        index_locations: &IndexLocations,
        hasher: &HashStrategy,
    ) -> IndexMap<Index, BTreeMap<Version, CachedRegistryDist>> {
        let mut map = IndexMap::new();

        // Collect into owned `IndexUrl`.
        let flat_index_urls: Vec<Index> = index_locations
            .flat_index()
            .map(|flat_index| Index::from_extra_index_url(IndexUrl::from(flat_index.clone())))
            .collect();

        for index in index_locations
            .allowed_indexes()
            .chain(flat_index_urls.iter())
        {
            let mut versions = BTreeMap::new();

            // Index all the wheels that were downloaded directly from the registry.
            let wheel_dir = cache.shard(
                CacheBucket::Wheels,
                WheelCache::Index(index.url()).wheel_dir(package.to_string()),
            );

            // For registry wheels, the cache structure is: `<index>/<package-name>/<wheel>.http`
            // or `<index>/<package-name>/<version>/<wheel>.rev`.
            for file in files(&wheel_dir) {
                match index.url() {
                    // Add files from remote registries.
                    IndexUrl::Pypi(_) | IndexUrl::Url(_) => {
                        if file
                            .extension()
                            .is_some_and(|ext| ext.eq_ignore_ascii_case("http"))
                        {
                            if let Some(wheel) =
                                CachedWheel::from_http_pointer(wheel_dir.join(file), cache)
                            {
                                // Enforce hash-checking based on the built distribution.
                                if wheel.satisfies(
                                    hasher
                                        .get_package(&wheel.filename.name, &wheel.filename.version),
                                ) {
                                    Self::add_wheel(wheel, tags, &mut versions);
                                }
                            }
                        }
                    }
                    // Add files from local registries (e.g., `--find-links`).
                    IndexUrl::Path(_) => {
                        if file
                            .extension()
                            .is_some_and(|ext| ext.eq_ignore_ascii_case("rev"))
                        {
                            if let Some(wheel) =
                                CachedWheel::from_local_pointer(wheel_dir.join(file), cache)
                            {
                                // Enforce hash-checking based on the built distribution.
                                if wheel.satisfies(
                                    hasher
                                        .get_package(&wheel.filename.name, &wheel.filename.version),
                                ) {
                                    Self::add_wheel(wheel, tags, &mut versions);
                                }
                            }
                        }
                    }
                }
            }

            // Index all the built wheels, created by downloading and building source distributions
            // from the registry.
            let cache_shard = cache.shard(
                CacheBucket::SourceDistributions,
                WheelCache::Index(index.url()).wheel_dir(package.to_string()),
            );

            // For registry wheels, the cache structure is: `<index>/<package-name>/<version>/`.
            for shard in directories(&cache_shard) {
                // Read the existing metadata from the cache, if it exists.
                let cache_shard = cache_shard.shard(shard);

                // Read the revision from the cache.
                let revision = match index.url() {
                    // Add files from remote registries.
                    IndexUrl::Pypi(_) | IndexUrl::Url(_) => {
                        let revision_entry = cache_shard.entry(HTTP_REVISION);
                        if let Ok(Some(pointer)) = HttpRevisionPointer::read_from(revision_entry) {
                            Some(pointer.into_revision())
                        } else {
                            None
                        }
                    }
                    // Add files from local registries (e.g., `--find-links`).
                    IndexUrl::Path(_) => {
                        let revision_entry = cache_shard.entry(LOCAL_REVISION);
                        if let Ok(Some(pointer)) = LocalRevisionPointer::read_from(revision_entry) {
                            Some(pointer.into_revision())
                        } else {
                            None
                        }
                    }
                };

                if let Some(revision) = revision {
                    for wheel_dir in symlinks(cache_shard.join(revision.id())) {
                        if let Some(wheel) = CachedWheel::from_built_source(wheel_dir) {
                            // Enforce hash-checking based on the source distribution.
                            if revision.satisfies(
                                hasher.get_package(&wheel.filename.name, &wheel.filename.version),
                            ) {
                                Self::add_wheel(wheel, tags, &mut versions);
                            }
                        }
                    }
                }
            }

            map.insert(index.clone(), versions);
        }

        map
    }

    /// Add the [`CachedWheel`] to the index.
    fn add_wheel(
        wheel: CachedWheel,
        tags: &Tags,
        versions: &mut BTreeMap<Version, CachedRegistryDist>,
    ) {
        let dist_info = wheel.into_registry_dist();

        // Pick the wheel with the highest priority
        let compatibility = dist_info.filename.compatibility(tags);
        if let Some(existing) = versions.get_mut(&dist_info.filename.version) {
            // Override if we have better compatibility
            if compatibility > existing.filename.compatibility(tags) {
                *existing = dist_info;
            }
        } else if compatibility.is_compatible() {
            versions.insert(dist_info.filename.version.clone(), dist_info);
        }
    }
}
