use std::{
    env,
    fs::{create_dir_all, rename, File},
    io::prelude::*,
    path::Path,
    sync::Arc,
    time::Duration,
};

use flate2::{write::GzEncoder, Compression};
use futures::{FutureExt, StreamExt};
use snafu::{OptionExt, ResultExt, Snafu};
use stackable_operator::{
    client,
    k8s_openapi::api::core::v1::ConfigMap,
    kube::{
        runtime::{controller::Action, watcher, Controller},
        Api,
    },
    logging::{
        controller::{report_controller_reconciled, ReconcilerError},
        TracingTarget,
    },
};
use strum::{EnumDiscriminants, IntoStaticStr};
use tar::Builder;
use warp::Filter;

type Result<T, E = Error> = std::result::Result<T, E>;

const OPERATOR_NAME: &str = "opa.stackable.tech";
const BUNDLE_BUILDER_CONTROLLER_NAME: &str = "bundlebuilder";

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("unable to create stackable-operator client"))]
    CreateClient {
        source: stackable_operator::client::Error,
    },
}

#[derive(Debug, EnumDiscriminants, Snafu)]
#[strum_discriminants(derive(IntoStaticStr))]
#[allow(clippy::enum_variant_names)]
pub enum ControllerError {
    #[snafu(display("opa bundle has no name"))]
    OpaBundleHasNoName,

    #[snafu(display("opa bundle dir error"))]
    OpaBundleDir { source: std::io::Error },

    #[snafu(display("missing namespace to watch"))]
    MissingWatchNamespace,

    #[snafu(display("could not create {path:?}"))]
    CreateBundle {
        source: std::io::Error,
        path: String,
    },

    #[snafu(display("could not create bundle tar"))]
    CreateBundleTar { source: std::io::Error },

    #[snafu(display("could not append to bundle tar"))]
    AppendToBundleTar { source: std::io::Error },
}

impl ReconcilerError for ControllerError {
    fn category(&self) -> &'static str {
        ControllerErrorDiscriminants::from(self).into()
    }
}
pub struct Ctx {
    pub active: String,
    pub incoming: String,
    pub tmp: String,
}

const WATCH_NAMESPACE_ENV: &str = "WATCH_NAMESPACE";
const BUNDLES_ACTIVE_DIR: &str = "/bundles/active";
const BUNDLES_INCOMING_DIR: &str = "/bundles/incoming";
const BUNDLES_TMP_DIR: &str = "/bundles/tmp";
const BUNDLE_NAME: &str = "bundle.tar.gz";

#[tokio::main]
async fn main() -> Result<()> {
    stackable_operator::logging::initialize_logging(
        "OPA_BUNDLE_BUILDER_LOG",
        "opa-bundle-builder",
        TracingTarget::None,
    );

    let client = client::create_client(Some(OPERATOR_NAME.to_string()))
        .await
        .context(CreateClientSnafu)?;

    match env::var(WATCH_NAMESPACE_ENV) {
        Ok(namespace) => {
            let configmaps_api: Api<ConfigMap> = client.get_api(namespace.as_ref());

            let web_server = make_web_server();

            let controller = Controller::new(
                configmaps_api,
                watcher::Config::default().labels(&format!("{OPERATOR_NAME}/bundle")),
            )
            .run(
                update_bundle,
                error_policy,
                Arc::new(Ctx {
                    active: BUNDLES_ACTIVE_DIR.to_string(),
                    incoming: BUNDLES_INCOMING_DIR.to_string(),
                    tmp: BUNDLES_TMP_DIR.to_string(),
                }),
            )
            .map(|res| {
                report_controller_reconciled(
                    &client,
                    &format!("{BUNDLE_BUILDER_CONTROLLER_NAME}.{OPERATOR_NAME}"),
                    &res,
                )
            });

            futures::stream::select(controller, web_server)
                .collect::<()>()
                .await;
        }
        Err(_) => {
            tracing::error!(
                "missing namespace to watch. Env var {WATCH_NAMESPACE_ENV:?} is probably not defined"
            );
        }
    }

    Ok(())
}

/// Create the web server for bundles.
///
/// There are two paths available:
/// - /opa/v1/opa/bundle.tar.gz
/// - /status
///
fn make_web_server() -> futures::future::IntoStream<impl futures::Future<Output = ()>> {
    let web_bundle = warp::path!("opa" / "v1" / "opa" / "bundle.tar.gz")
        .and(warp::fs::file(format!(
            "{BUNDLES_ACTIVE_DIR}/{BUNDLE_NAME}"
        )))
        .with(warp::log("bundle"));
    let web_status = warp::path("status")
        .map(|| "i'm good")
        .with(warp::log("status"));

    warp::serve(warp::get().and(web_bundle.or(web_status)))
        .run(([0, 0, 0, 0], 3030))
        .into_stream()
}

/// Updates the `/bundles/active/bundle.tar.gz` with the new `ConfigMap`.
///
/// All `ConfigMap`s are stored under [`BUNDLES_INCOMING_DIR`] and archived into [`BUNDLES_TMP_DIR`]/bundle.tar.gz first
/// before being moved to to [`BUNDLES_ACTIVE_DIR`]/bundle.tar.gz for serving.
///
/// The root of the tar file is always "bundles".
async fn update_bundle(bundle: Arc<ConfigMap>, ctx: Arc<Ctx>) -> Result<Action, ControllerError> {
    let name = bundle
        .metadata
        .name
        .as_ref()
        .context(OpaBundleHasNoNameSnafu)?;

    match bundle.data.as_ref() {
        Some(rules) => {
            let incoming = ctx.incoming.as_str();
            let active = ctx.active.as_str();
            let tmp = ctx.tmp.as_str();

            let temp_full_path = Path::new(incoming).join(Path::new(name.as_str()));
            create_dir_all(&temp_full_path).with_context(|_| OpaBundleDirSnafu)?;

            for (k, v) in rules.iter() {
                let rego_file_path = temp_full_path.clone().join(Path::new(k));

                File::create(&rego_file_path)
                    .and_then(|mut file| file.write_all(v.as_bytes()))
                    .context(OpaBundleDirSnafu)?;
            }

            let tmp_bundle_path = format!("{tmp}/{BUNDLE_NAME}");
            let tar_gz = File::create(&tmp_bundle_path).with_context(|_| CreateBundleSnafu {
                path: tmp_bundle_path.to_string(),
            })?;
            let gz_encoder = GzEncoder::new(tar_gz, Compression::best());
            let mut tar_builder = Builder::new(gz_encoder);

            tar_builder
                .append_dir_all("bundles", incoming)
                .context(AppendToBundleTarSnafu)?;
            tar_builder.finish().context(CreateBundleTarSnafu)?;

            let dest_path = Path::new(active).join(Path::new(BUNDLE_NAME));
            rename(Path::new(&tmp_bundle_path), dest_path).context(OpaBundleDirSnafu)?;
        }
        None => tracing::error!("empty config map {}", name),
    }

    Ok(Action::await_change())
}

pub fn error_policy<T>(_obj: Arc<T>, _error: &ControllerError, _ctx: Arc<Ctx>) -> Action {
    Action::requeue(Duration::from_secs(5))
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{create_dir, metadata},
        sync::Arc,
    };

    use stackable_operator::builder::{configmap::ConfigMapBuilder, meta::ObjectMetaBuilder};
    use tempfile::TempDir;

    use super::update_bundle;
    use crate::Ctx;

    #[test]
    pub fn test_update_bundle() {
        let tmp = TempDir::new().unwrap();
        let active = tmp.path().join("active");
        let incoming = tmp.path().join("incoming");
        let tmp = tmp.path().join("tmp");

        create_dir(&active).unwrap();
        create_dir(&incoming).unwrap();
        create_dir(&tmp).unwrap();

        let config_map = ConfigMapBuilder::new()
            .metadata(ObjectMetaBuilder::new().name("test-bundle-builder").build())
            .add_data(String::from("roles.rego"), String::from("allow user true"))
            .build()
            .unwrap();

        let context = Arc::new(Ctx {
            active: String::from(active.to_str().unwrap()),
            incoming: String::from(incoming.to_str().unwrap()),
            tmp: String::from(tmp.to_str().unwrap()),
        });

        match tokio_test::block_on(update_bundle(Arc::new(config_map), context)) {
            Ok(_) => assert!(metadata(active.join("bundle.tar.gz")).unwrap().is_file()),
            Err(e) => panic!("{:?}", e),
        }
    }
}
