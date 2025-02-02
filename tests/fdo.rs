use std::env::temp_dir;

use anyhow::ensure;
use busd::bus::Bus;
use ntest::timeout;
use rand::{
    distributions::{Alphanumeric, DistString},
    thread_rng,
};
use tokio::{select, sync::oneshot::Sender};
use tracing::instrument;
use zbus::{
    fdo::{DBusProxy, ReleaseNameReply, RequestNameFlags, RequestNameReply},
    names::WellKnownName,
    AuthMechanism, CacheProperties, ConnectionBuilder,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[instrument]
#[timeout(15000)]
async fn name_ownership_changes() {
    busd::tracing_subscriber::init();

    // Unix socket
    #[cfg(unix)]
    {
        let s = Alphanumeric.sample_string(&mut thread_rng(), 10);
        let path = temp_dir().join(s);
        let address = format!("unix:path={}", path.display());
        name_ownership_changes_(&address, AuthMechanism::External).await;
    }

    // TCP socket
    let address = format!("tcp:host=127.0.0.1,port=4242");
    name_ownership_changes_(&address, AuthMechanism::Cookie).await;
    name_ownership_changes_(&address, AuthMechanism::Anonymous).await;
}

async fn name_ownership_changes_(address: &str, auth_mechanism: AuthMechanism) {
    let mut bus = Bus::for_address(Some(address), auth_mechanism)
        .await
        .unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();

    let handle = tokio::spawn(async move {
        select! {
            _ = rx => (),
            res = bus.run() => match res {
                Ok(()) => panic!("Bus exited unexpectedly"),
                Err(e) => panic!("Bus exited with an error: {}", e),
            }
        }

        bus
    });

    let ret = name_ownership_changes_client(address, tx).await;
    let bus = handle.await.unwrap();
    bus.cleanup().await.unwrap();
    ret.unwrap();
}

#[instrument]
async fn name_ownership_changes_client(address: &str, tx: Sender<()>) -> anyhow::Result<()> {
    let conn = ConnectionBuilder::address(address)?.build().await?;
    let dbus_proxy = DBusProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let name: WellKnownName = "org.blah".try_into()?;

    // This should work.
    let ret = dbus_proxy
        .request_name(name.clone(), RequestNameFlags::AllowReplacement.into())
        .await?;
    ensure!(
        ret == RequestNameReply::PrimaryOwner,
        "expected to become primary owner"
    );

    // This shouldn't and we should be told we already own the name.
    let ret = dbus_proxy
        .request_name(name.clone(), RequestNameFlags::AllowReplacement.into())
        .await?;
    ensure!(
        ret == RequestNameReply::AlreadyOwner,
        "expected to be already primary owner"
    );

    // Now we try with another connection and we should be queued.
    let conn2 = ConnectionBuilder::address(address)?.build().await?;
    let dbus_proxy2 = DBusProxy::builder(&conn2)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let ret = dbus_proxy2
        .request_name(name.clone(), Default::default())
        .await?;

    // Check that first client is the primary owner before it releases the name.
    ensure!(ret == RequestNameReply::InQueue, "expected to be in queue");
    let owner = dbus_proxy.get_name_owner(name.clone().into()).await?;
    ensure!(owner == *conn.unique_name().unwrap(), "unexpected owner");

    // Now the first client releases name.
    let ret = dbus_proxy.release_name(name.clone()).await?;
    ensure!(
        ret == ReleaseNameReply::Released,
        "expected name to be released"
    );

    // Now the second client should be the primary owner.
    let owner = dbus_proxy.get_name_owner(name.clone().into()).await?;
    ensure!(owner == *conn2.unique_name().unwrap(), "unexpected owner");

    tx.send(()).unwrap();

    Ok(())
}
