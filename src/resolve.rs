use crate::prelude::*;
use elsa::FrozenMap;
use pubgrub::range::Range;
use pubgrub::report::DerivationTree;
use pubgrub::report::Reporter;
use pubgrub::solver::{Dependencies, DependencyConstraints};
use std::borrow::Borrow;

use crate::package_db::{ArtifactInfo, PackageDB};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "AllowPreSerdeHelper", into = "AllowPreSerdeHelper")]
pub enum AllowPre {
    Some(HashSet<PackageName>),
    All,
}

impl AllowPre {
    pub fn allow_pre_for(&self, package: &PackageName) -> bool {
        match &self {
            AllowPre::Some(pkgs) => pkgs.contains(&package),
            AllowPre::All => true,
        }
    }
}

impl Default for AllowPre {
    fn default() -> Self {
        AllowPre::Some(HashSet::new())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum AllowPreSerdeHelper<'a> {
    Some(HashSet<PackageName>),
    Other(&'a str),
}

impl<'a> TryFrom<AllowPreSerdeHelper<'a>> for AllowPre {
    type Error = eyre::Report;

    fn try_from(value: AllowPreSerdeHelper) -> Result<Self, Self::Error> {
        match value {
            AllowPreSerdeHelper::Some(pkgs) => Ok(AllowPre::Some(pkgs)),
            AllowPreSerdeHelper::Other(value) => {
                if value == ":all:" {
                    Ok(AllowPre::All)
                } else {
                    bail!("expected a list of packages or the magic string ':all:'")
                }
            }
        }
    }
}

impl<'a> From<AllowPre> for AllowPreSerdeHelper<'a> {
    fn from(value: AllowPre) -> Self {
        match value {
            AllowPre::Some(pkgs) => AllowPreSerdeHelper::Some(pkgs),
            AllowPre::All => AllowPreSerdeHelper::Other(":all:"),
        }
    }
}

fn allow_pre_is_empty(value: &AllowPre) -> bool {
    if let AllowPre::Some(pkgs) = value {
        pkgs.is_empty()
    } else {
        false
    }
}

/// A high-level description of an environment that a user would like to be able to
/// build. Doesn't necessarily have to be what the user types in exactly, but has to
/// represent their intentions, and *not* anything that requires looking at a package
/// index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Brief {
    pub python: PythonRequirement,
    // don't need python_constraints because we always install exactly one python
    pub requirements: Vec<UserRequirement>,
    #[serde(skip_serializing_if = "allow_pre_is_empty")]
    pub allow_pre: AllowPre,
    // XX TODO
    //pub constraints: Vec<UserRequirement>,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct PinnedPackage {
    pub name: PackageName,
    pub version: Version,
    pub hashes: Vec<ArtifactHash>,
}

impl Display for PinnedPackage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} (with {} known hashes)",
            self.name.as_given(),
            self.version,
            self.hashes.len()
        )
    }
}

struct VersionHints<'a>(
    HashMap<&'a PackageName, (&'a Version, HashSet<&'a ArtifactHash>)>,
);

impl<'a> VersionHints<'a> {
    fn new() -> VersionHints<'a> {
        VersionHints(HashMap::new())
    }

    fn add_pinned(&mut self, pin: &'a PinnedPackage) {
        self.0
            .insert(&pin.name, (&pin.version, pin.hashes.iter().collect()));
    }

    fn from(blueprint: &'a Blueprint) -> VersionHints<'a> {
        let mut hints = VersionHints::new();
        hints.add_pinned(&blueprint.pybi);
        for (wheel, _) in &blueprint.wheels {
            hints.add_pinned(&wheel);
        }
        hints
    }
}

/// This is the subset of WheelCoreMetadata that the resolver actually uses.
///
/// As part of resolving a Brief -> a Blueprint, for each package+version, we need to
/// get the core metadata, which we get from a wheel. But when we do this, we have to
/// pick a *specific* wheel to get the metadata from. But we want our Blueprint to be
/// usable across multiple platforms. So when we go to install it, we might decide to
/// install a different wheel for that package+version. And that different wheel *might*
/// have different core metadata in it! And if it does, then our Blueprint might no
/// longer generate a valid environment!
///
/// Hopefully this never happens – all wheels for a given package+version *should* have
/// the same metadata (or at least, the parts of the metadata that actually feed into
/// the resolution algorithm). But if it does happen, we want to detect it and give a
/// diagnostic, not just blithely create an invalid environment. So we pull out the
/// resolver-relevant metadata here, so we can store it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WheelResolveMetadata {
    pub provenance: String,
    pub inner: WheelResolveMetadataInner,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct WheelResolveMetadataInner {
    pub requires_dist: Vec<PackageRequirement>,
    pub requires_python: Specifiers,
    pub extras: HashSet<Extra>,
}

impl WheelResolveMetadata {
    pub fn from(ai: &ArtifactInfo, m: &WheelCoreMetadata) -> WheelResolveMetadata {
        let provenance = ai.url.to_string();
        let inner = WheelResolveMetadataInner {
            requires_dist: m.requires_dist.clone(),
            requires_python: m.requires_python.clone(),
            extras: m.extras.clone(),
        };
        WheelResolveMetadata { provenance, inner }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Blueprint {
    pub pybi: PinnedPackage,
    // XX TODO: all marker settings relied on when computing this
    pub wheels: Vec<(PinnedPackage, WheelResolveMetadata)>,
}

impl Display for Blueprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pybi: {}\n", self.pybi)?;
        for (wheel, em) in &self.wheels {
            write!(f, "wheel: {} (metadata from {})\n", wheel, em.provenance)?;
        }
        Ok(())
    }
}

fn pick_best_pybi<'a>(
    artifact_infos: &'a [ArtifactInfo],
    platform: &PybiPlatform,
) -> Option<&'a ArtifactInfo> {
    artifact_infos
        .iter()
        .filter_map(|ai| {
            if let ArtifactName::Pybi(name) = &ai.name {
                platform
                    .max_compatibility(name.arch_tags.iter())
                    .map(|score| (ai, score))
            } else {
                None
            }
        })
        .max_by_key(|(_, score)| *score)
        .map(|(ai, _)| ai)
}

// XX TODO: merge with version preference logic in resolve.rs, b/c this should have
// similar handling of prereleases, yanks, previous-blueprint-hints, etc.
fn resolve_pybi<'a>(
    db: &'a PackageDB,
    brief: &Brief,
    platform: &PybiPlatform,
    hints: &VersionHints,
) -> Result<&'a ArtifactInfo> {
    let name = &brief.python.name;
    let versions = fetch_and_sort_versions(&db, &brief, &name, None, hints)?;
    for version in versions.iter() {
        if brief.python.specifiers.satisfied_by(&version)? {
            let artifact_infos = db.artifacts_for_version(&name, version)?;
            if let Some(ai) = pick_best_pybi(&artifact_infos, platform) {
                return Ok(ai);
            }
        }
    }
    bail!("no compatible pybis found for requirement and platform");
}

fn pinned(
    db: &PackageDB,
    name: PackageName,
    version: Version,
) -> Result<PinnedPackage> {
    let hashes = db
        .artifacts_for_version(&name, &version)?
        .iter()
        .filter_map(|ai| ai.hash.clone())
        .collect::<Vec<_>>();
    Ok(PinnedPackage {
        name,
        version,
        hashes,
    })
}

impl Brief {
    pub fn resolve(
        &self,
        db: &PackageDB,
        platform: &PybiPlatform,
        like: Option<&Blueprint>,
    ) -> Result<Blueprint> {
        let version_hints = like
            .map(VersionHints::from)
            .unwrap_or_else(VersionHints::new);
        let pybi_ai = resolve_pybi(&db, &self, &platform, &version_hints)?;
        let (_, pybi_metadata) = db
            .get_metadata::<Pybi, _>(&[pybi_ai])
            .wrap_err_with(|| format!("fetching metadata for {}", pybi_ai.url))?;
        let pybi_name = pybi_ai.name.inner_as::<PybiName>().unwrap();

        let mut env_marker_vars = pybi_metadata.environment_marker_variables.clone();
        if !env_marker_vars.contains_key("platform_machine") {
            let wheel_platform =
                platform.wheel_platform_for_pybi(&pybi_name, &pybi_metadata)?;
            env_marker_vars.insert(
                "platform_machine".to_string(),
                wheel_platform.infer_platform_machine()?.to_string(),
            );
        }

        let resolved_wheels =
            resolve_wheels(db, &self, &env_marker_vars, &version_hints)?;
        let mut wheels = Vec::<(PinnedPackage, WheelResolveMetadata)>::new();
        for (p, v, em) in resolved_wheels {
            wheels.push((pinned(&db, p, v)?, em));
        }

        Ok(Blueprint {
            pybi: pinned(
                &db,
                pybi_name.distribution.to_owned(),
                pybi_name.version.to_owned(),
            )?,
            wheels,
        })
    }
}

struct PubgrubState<'a> {
    // These are inputs to the resolve process
    db: &'a PackageDB,
    env: &'a HashMap<String, String>,
    brief: &'a Brief,
    version_hints: &'a VersionHints<'a>,

    python_full_version: Version,
    // record of the metadata we used, so we can record it and validate it later when
    // using the pins
    expected_metadata: FrozenMap<(PackageName, Version), Box<WheelResolveMetadata>>,
    // These are sorted with most-preferred first.
    versions: FrozenMap<PackageName, Vec<&'a Version>>,
}

fn get_or_fill<'a, 'b, K, V, F>(
    map: &'a FrozenMap<K, V>,
    key: &'b K,
    f: F,
) -> Result<&'a V::Target>
where
    K: Eq + std::hash::Hash + Clone,
    F: FnOnce() -> Result<V>,
    V: stable_deref_trait::StableDeref,
{
    if let Some(v) = map.get(key) {
        Ok(v)
    } else {
        Ok(map.insert(key.to_owned(), f()?))
    }
}

fn fetch_and_sort_versions<'a>(
    db: &'a PackageDB,
    brief: &Brief,
    package: &PackageName,
    python_version: Option<&Version>,
    hints: &VersionHints,
) -> Result<Vec<&'a Version>> {
    let artifacts = db.available_artifacts(&package)?;
    let mut versions = Vec::new();
    let allow_prerelease = brief.allow_pre.allow_pre_for(&package);
    let (version_hint, hash_hints) = match hints.0.get(&package) {
        Some((version, hash)) => (Some(version), Some(hash)),
        None => (None, None),
    };
    for (version, ais) in artifacts.iter() {
        if !allow_prerelease && version.is_prerelease() {
            continue;
        }
        for ai in ais {
            if ai.yanked.yanked {
                let is_pinned = match (&hash_hints, &ai.hash) {
                    (Some(hints), Some(hash)) => hints.contains(&hash),
                    _ => false,
                };
                if !is_pinned {
                    continue;
                }
            }
            if let (Some(python_version), Some(requires_python)) =
                (python_version, &ai.requires_python)
            {
                let requires_python: Specifiers = requires_python.parse()?;
                if !requires_python.satisfied_by(&python_version)? {
                    continue;
                }
            }
            // we found a valid artifact for this version. So this version is valid, and
            // we can save it and move on to the next.
            versions.push(version);
            break;
        }
    }
    // sort from highest to lowest
    versions.sort_unstable_by_key(|v| {
        (
            // false sorts before true, so version_hint = v sorts first
            version_hint != Some(&v),
            // and otherwise, high versions come before low versions
            std::cmp::Reverse(*v),
        )
    });

    Ok(versions)
}

impl<'a> PubgrubState<'a> {
    fn metadata(
        &self,
        release: &(PackageName, Version),
    ) -> Result<&WheelResolveMetadataInner> {
        Ok(&get_or_fill(&self.expected_metadata, release, || {
            let ais = self.db.artifacts_for_version(&release.0, &release.1)?;
            let (ai, wheel_metadata) = self.db.get_metadata::<Wheel, _>(ais)?;
            Ok(Box::new(WheelResolveMetadata::from(&ai, &wheel_metadata)))
        })?
        .inner)
    }

    fn versions(&self, package: &PackageName) -> Result<&[&Version]> {
        get_or_fill(&self.versions, &package, || {
            fetch_and_sort_versions(
                &self.db,
                &self.brief,
                &package,
                Some(&self.python_full_version),
                &self.version_hints,
            )
        })
    }
}

fn resolve_wheels(
    db: &PackageDB,
    brief: &Brief,
    env: &HashMap<String, String>,
    version_hints: &VersionHints,
) -> Result<Vec<(PackageName, Version, WheelResolveMetadata)>> {
    let state = PubgrubState {
        db,
        brief,
        env,
        version_hints,
        python_full_version: env
            .get("python_full_version")
            .ok_or(eyre!(
                "Missing 'python_full_version' environment marker variable"
            ))?
            .parse()?,
        expected_metadata: Default::default(),
        versions: Default::default(),
    };

    // XX this error reporting is terrible. It's a hack to work around PubGrubError not
    // being convertible to eyre::Report, because eyre::Report requires Send.
    let result = pubgrub::solver::resolve(&state, ResPkg::Root, ROOT_VERSION.clone());

    use pubgrub::error::PubGrubError::*;

    match result {
        Ok(solution) => Ok(solution
            .into_iter()
            .filter_map(|(pkg, v)| match pkg {
                ResPkg::Root => None,
                ResPkg::Package(_, Some(_)) => None,
                ResPkg::Package(name, None) => Some({
                    (
                        name.clone(),
                        v.clone(),
                        state.expected_metadata.get(&(name, v)).unwrap().clone(),
                    )
                }),
            })
            .collect()),
        Err(err) => Err(match err {
            ErrorRetrievingDependencies {
                package,
                version,
                source,
            } => {
                context!("fetching dependencies of {} v{}", package, version);
                eyre!("{}", source)
            }
            ErrorChoosingPackageVersion(boxed_err) => {
                eyre!("{}", boxed_err.to_string())
            }
            ErrorInShouldCancel(boxed_err) => eyre!("{}", boxed_err.to_string()),
            Failure(s) => eyre!("{}", s),
            // XX Maybe the empty-range and self-dependency cases should be filtered out
            // inside our code, for robustness?
            DependencyOnTheEmptySet {
                package,
                version,
                dependent,
            } => eyre!(
                "{} v{}'s dependency on {} has self-contradictory version ranges",
                package,
                version,
                dependent
            ),
            SelfDependency { package, version } => {
                eyre!("{} v{} depends on itself", package, version)
            }

            NoSolution(mut derivation_tree) => {
                fn dump_tree(tree: &DerivationTree<ResPkg, Version>, depth: usize) {
                    let indent = "   ".repeat(depth);
                    match tree {
                        DerivationTree::External(inner) => {
                            println!("{}external: {}", indent, inner);
                        }
                        DerivationTree::Derived(inner) => {
                            println!("{}derived (id={:?})", indent, inner.shared_id);
                            for (pkg, term) in inner.terms.iter() {
                                println!("{}  {} -> {}", indent, pkg, term);
                            }
                            println!("{}cause 1:", indent);
                            dump_tree(&inner.cause1, depth + 1);
                            println!("{}cause 2:", indent);
                            dump_tree(&inner.cause2, depth + 1);
                        }
                    }
                }

                println!("\n-------- derivation tree --------");
                //println!("{:?}", derivation_tree);
                dump_tree(&derivation_tree, 0);
                derivation_tree.collapse_no_versions();
                println!("\n-------- derivation tree (collapsed) --------");
                //println!("{:?}", derivation_tree);
                dump_tree(&derivation_tree, 0);
                eyre!(
                    "{}",
                    pubgrub::report::DefaultStringReporter::report(&derivation_tree)
                )
            }
        }),
    }
}

struct HashMapEnv<'a> {
    basic_env: &'a HashMap<String, String>,
    extra: Option<&'a str>,
}

impl<'a> marker::Env for HashMapEnv<'a> {
    fn get_marker_var(&self, var: &str) -> Option<&str> {
        match var {
            // we want 'extra' to have some value, because looking it up shouldn't be an
            // error. But we want that value to be something that will never match a
            // real extra. We use an empty string.
            "extra" => Some(self.extra.unwrap_or("")),
            _ => self.basic_env.get(var).map(|s| s.as_str()),
        }
    }
}

// A "package" for purposes of resolving. This is an extended version of what PyPI
// considers a package, in two ways.
//
// First, the pubgrub crate assumes that resolution always starts with a single required
// package==version. So we make a virtual "root" package, pass that to pubgrub as our
// initial requirement, and then we tell pubgrub that Root package depends on our actual
// requirements. (It'd be nice if pubgrub just took a DependencyConstraints to start
// with, but, whatever.)
//
// Second, extras. To handle them properly, we create virtual packages for each extra.
// So e.g. "foo[bar,baz]" really means "foo, but with the [bar] and [baz] requirements
// added to its normal set". But that's not a concept that pubgrub understands. So
// instead, we pretend that there are two packages "foo[bar]" and "foo[baz]", and their
// requirements are:
//
// - the requirements of 'foo', when evaluated with the appropriate 'extra' set
// - a special requirement on itself 'foo', with the exact same version.
//
// Result: if we wanted "foo[bar,baz]", we end up with "foo", plus all the [bar] and
// [baz] requirements at the same version. So at the end, we can just go through and
// discard all the virtual extra packages, to get the real set of packages.
//
// This trick is stolen from pip's resolver. It also puts us in a good place if reified
// extras[1] ever become a thing, because we're basically reifying them already.
//
// [1] https://mail.python.org/pipermail/distutils-sig/2015-October/027364.html
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum ResPkg {
    Root,
    Package(PackageName, Option<Extra>),
}

static ROOT_VERSION: Lazy<Version> = Lazy::new(|| "0".try_into().unwrap());

impl Display for ResPkg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResPkg::Root => write!(f, "<root>"),
            ResPkg::Package(name, None) => write!(f, "{}", name.as_given()),
            ResPkg::Package(name, Some(extra)) => {
                write!(f, "{}[{}]", name.as_given(), extra.as_given())
            }
        }
    }
}

impl<'a> PubgrubState<'a> {
    fn requirements_to_pubgrub<'r, R, I>(
        &self,
        reqs: I,
        dc: &mut DependencyConstraints<ResPkg, Version>,
        extra: Option<&Extra>,
    ) -> Result<()>
    where
        R: std::ops::Deref<Target = Requirement> + 'r,
        I: Iterator<Item = &'r R>,
    {
        let env = HashMapEnv {
            basic_env: &self.env,
            extra: extra.map(|e| e.normalized()),
        };

        for req in reqs {
            if let Some(expr) = &req.env_marker_expr {
                if !expr.eval(&env)? {
                    continue;
                }
            }

            let mut maybe_extras: Vec<Option<Extra>> =
                req.extras.iter().map(|e| Some(e.clone())).collect();
            if maybe_extras.is_empty() {
                maybe_extras.push(None);
            }

            for maybe_extra in maybe_extras {
                let pkg = ResPkg::Package(req.name.clone(), maybe_extra);
                let range = specifiers_to_pubgrub(&req.specifiers)?;
                trace!("adding dependency: {} {}", pkg, range);
                dc.insert(pkg, range);
            }
        }
        Ok(())
    }
}

fn specifiers_to_pubgrub(specs: &Specifiers) -> Result<Range<Version>> {
    let mut final_range = Range::any();
    for spec in &specs.0 {
        let spec_range =
            spec.to_ranges()?
                .into_iter()
                .fold(Range::none(), |accum, r| {
                    accum.union(&if r.end < *VERSION_INFINITY {
                        Range::between(r.start, r.end)
                    } else {
                        Range::higher_than(r.start)
                    })
                });
        final_range = final_range.intersection(&spec_range);
    }
    Ok(final_range)
}

impl<'a> pubgrub::solver::DependencyProvider<ResPkg, Version> for PubgrubState<'a> {
    fn choose_package_version<T, U>(
        &self,
        mut potential_packages: impl Iterator<Item = (T, U)>,
    ) -> Result<(T, Option<Version>), Box<dyn std::error::Error>>
    where
        T: Borrow<ResPkg>,
        U: Borrow<Range<Version>>,
    {
        trace!("----> pubgrub called choose_package_version");
        // XX TODO: laziest possible heuristic, just pick the first package offered
        let (respkg, range) = potential_packages.next().unwrap();

        match respkg.borrow() {
            ResPkg::Root => {
                trace!("<---- decision: root package magic version 0");
                Ok((respkg, Some(ROOT_VERSION.clone())))
            }
            ResPkg::Package(name, _) => {
                for &version in self.versions(&name)?.iter() {
                    if !range.borrow().contains(version) {
                        trace!("Version {} is out of range", version);
                        continue;
                    }

                    let metadata = self.metadata(&(name.clone(), version.clone()))?;
                    if !metadata
                        .requires_python
                        .satisfied_by(&self.python_full_version)?
                    {
                        Err(eyre!(
                            "{} {}: bad requires-python, but pypi didn't tell us!",
                            name.as_given(),
                            version
                        ))?;
                    }
                    trace!("<---- decision: {} {}", respkg.borrow(), version);
                    return Ok((respkg, Some(version.clone())));
                }

                trace!(
                    "<---- decision: no versions of {} in range",
                    respkg.borrow()
                );
                Ok((respkg, None))
            }
        }
    }

    fn get_dependencies(
        &self,
        pkg: &ResPkg,
        version: &Version,
    ) -> Result<
        pubgrub::solver::Dependencies<ResPkg, Version>,
        Box<dyn std::error::Error>,
    > {
        trace!("----> pubgrub called get_dependencies {} v{}", pkg, version);

        match pkg {
            ResPkg::Root => {
                let mut dc: DependencyConstraints<ResPkg, Version> =
                    vec![].into_iter().collect();
                self.requirements_to_pubgrub(
                    self.brief.requirements.iter(),
                    &mut dc,
                    None,
                )?;
                trace!("<---- dependencies complete");
                Ok(Dependencies::Known(dc))
            }
            ResPkg::Package(name, extra) => {
                let metadata = self.metadata(&(name.clone(), version.clone()))?;

                let mut dc: DependencyConstraints<ResPkg, Version> = Default::default();

                self.requirements_to_pubgrub(
                    metadata.requires_dist.iter(),
                    &mut dc,
                    extra.as_ref(),
                )?;

                if let Some(inner) = extra {
                    if !metadata.extras.contains(inner) {
                        Err(eyre!(
                            "package {} has no extra [{}]",
                            name.as_given(),
                            inner.as_given()
                        ))?;
                    }
                    dc.insert(
                        ResPkg::Package(name.clone(), None),
                        Range::exact(version.clone()),
                    );
                }

                trace!("<---- dependencies complete");
                Ok(Dependencies::Known(dc))
            }
        }
    }
}
