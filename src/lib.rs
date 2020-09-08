// SPDX-License-Identifier: Apache-2.0

use serde_derive::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use structopt::StructOpt;
use tokio::sync::Mutex;

pub const WASM_RUNTIME_BINARY_PATH: &str =
    "/home/mike/programming/enarx-wasmldr/target/x86_64-unknown-linux-musl/debug/enarx-wasmldr";
pub const LOCAL_LISTEN_ADDRESS: &str = "192.168.1.202";

pub const PROTO_VERSION: f32 = 0.1;
pub const PROTO_NAME: &str = "Enarx-Keep-Manager";
pub const BIND_PORT: u16 = 3030;

pub const KEEP_LOADER_STATE_UNDEF: u8 = 0;
pub const KEEP_LOADER_STATE_LISTENING: u8 = 1;
pub const KEEP_LOADER_STATE_STARTED: u8 = 2;
pub const KEEP_LOADER_STATE_COMPLETE: u8 = 3;
pub const KEEP_LOADER_STATE_ERROR: u8 = 15;

pub const KEEP_INFO_COMMAND: &str = "keep-info";
pub const CONTRACT_COMMAND: &str = "command";
pub const KEEP_COMMAND: &str = "command";
pub const KEEP_AUTH: &str = "auth-token";
pub const KEEP_PORT: &str = "keep-port";
pub const KEEP_ADDR: &str = "keep-addr";
pub const KEEP_KUUID: &str = "kuuid";
pub const KEEP_ARCH: &str = "keep-arch";
pub const KEEP_ARCH_WASI: &str = "wasi";
pub const KEEP_ARCH_SEV: &str = "AMD-SEV";
pub const KEEP_ARCH_SGX: &str = "Intel-SGX";
//pub const KEEP_APP_LOADER_BIND_PORT: &str = "app-loader-bind-port";
pub const APP_LOADER_BIND_PORT_START: u16 = 3031;
pub const KEEP_APP_LOADER_START_COMMAND: &str = "apploader-start";
pub const KEEP_APP_LOADER_ADDR: &str = "apploader-addr";
pub const KEEP_APP_LOADER_PORT: &str = "apploader-port";
pub const KEEP_BACKEND_SET: &str = "keep-backend-set";
pub const KEEP_TYPE_INFO_COMMAND: &str = "keep-path-info";
pub const KEEP_PREATT_SOCK_COMMAND: &str = "keep-preatt-sock";
pub const KEEP_PAYLOAD_COMMAND: &str = "keep-payload";

pub type KeepLoaderList = Arc<Mutex<Vec<KeepLoader>>>;

#[derive(Serialize, Deserialize, Clone)]
pub struct KeepLoader {
    //TODO - some of these might be better as options
    pub backend_type: String,
    pub state: u8,
    pub kuuid: usize,
    pub app_loader_bind_port: u16,
    pub bindaddress: String,
    pub exec: Option<Exec>,
    //we may wish to add information here about whether we're happy to share
    // all of this information with external parties, but since the keeploader
    // is operating outside the TEE boundary, there's only so much we can do
    // to keep this information confidential
}

/// Executes a keep
#[derive(StructOpt, Serialize, Deserialize, Clone)]
pub struct Exec {
    /// The socket to use for preattestation
    #[structopt(short, long)]
    pub sock: Option<PathBuf>,

    /// The payload to run inside the keep
    pub code: PathBuf,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct JsonCommand {
    pub commandtype: String,
    pub commandcontents: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct KeepLoaderVec {
    pub klvec: Vec<KeepLoader>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct UndefinedReply {
    pub text: String,
}
