use std::collections::vec_deque::VecDeque;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use codec::Encode;

use ecies_ed25519::encrypt;
use jsonrpsee::core::client::ClientT;
use jsonrpsee::core::{Error, JsonValue};
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use jsonrpsee::server::ServerBuilder;
use jsonrpsee::types::Params;
use jsonrpsee::{rpc_params, RpcModule};
use primitive_types::{H256, U256};
use rand::{rngs::StdRng, SeedableRng};
use serde::Serialize;
use schnorrkel::{SecretKey, MiniSecretKey, ExpansionMode, Signature};

const LISTEN_ADDR: &'static str = "127.0.0.1:9833";

#[derive(Clone)]
pub(crate) struct MiningParams {
    pub(crate) pre_hash: H256,
    pub(crate) parent_hash: H256,
    pub(crate) win_dfclty: U256,
    pub(crate) pow_dfclty: U256,
    pub(crate) pub_key: ecies_ed25519::PublicKey,
}

#[derive(Clone, Encode)]
pub(crate) enum AlgoType {
    Grid2d,
    Grid2dV2,
    Grid2dV3,
}

impl AlgoType {
    pub(crate) fn as_p3d_algo(&self) -> p3d::AlgoType {
        match self {
            Self::Grid2d => p3d::AlgoType::Grid2d,
            Self::Grid2dV2 => p3d::AlgoType::Grid2dV2,
            Self::Grid2dV3 => p3d::AlgoType::Grid2dV3,
        }
    }

    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Grid2d => "Grid2d",
            Self::Grid2dV2 => "Grid2dV2",
            Self::Grid2dV3 => "Grid2dV3",
        }
    }
}

#[derive(Clone)]
pub(crate) struct P3dParams {
    pub(crate) algo: AlgoType,
    pub(crate) grid: usize,
    pub(crate) sect: usize,
}

impl P3dParams {
    pub(crate) fn new(ver: &str) -> Self {
        let grid = 8;
        let (algo, sect) = if ver == "grid2d" {
            (AlgoType::Grid2d, 66)
        } else if ver == "grid2d_v2" {
            (AlgoType::Grid2dV2, 12)
        } else if ver == "grid2d_v3" {
            (AlgoType::Grid2dV3, 12)
        } else {
            panic!("Unknown algorithm: {}", ver)
        };

        Self { algo, grid, sect }
    }
}

pub(crate) struct MiningObj {
    pub(crate) obj_id: u64,
    pub(crate) obj: Vec<u8>,
}

pub(crate) struct MiningProposal {
    pub(crate) params: MiningParams,
    pub(crate) hash: H256,
    pub(crate) obj_id: u64,
    pub(crate) obj: Vec<u8>,
}

#[derive(Serialize)]
pub(crate) struct Payload {
    pub(crate) pool_id:     String,
    pub(crate) member_id:   String,
    pub(crate) pre_hash:    H256,
    pub(crate) parent_hash: H256,
    pub(crate) algo:        String,
    pub(crate) dfclty:      U256,
    pub(crate) hash:        H256,
    pub(crate) obj_id:      u64,
    pub(crate) obj:         Vec<u8>,
}

pub(crate) struct MiningContext {
    pub(crate) p3d_params: P3dParams,
    pub(crate) pool_id: String,
    pub(crate) member_id: String,
    pub(crate) key: SecretKey,
    pub(crate) cur_state: Mutex<Option<MiningParams>>,
    pub(crate) in_queue: Mutex<VecDeque<MiningObj>>,
    pub(crate) out_queue: Mutex<VecDeque<MiningProposal>>,

    pub(crate) client: HttpClient,
}

impl MiningContext {
    pub(crate) fn new(
        p3d_params: P3dParams,
        pool_addr: &str,
        pool_id: String,
        member_id: String,
        key: String,
    ) -> anyhow::Result<Self> {

        let key = key.replacen("0x", "", 1);
        let key_data = hex::decode(&key[..])?;
        let key = MiniSecretKey::from_bytes(&key_data[..])
            .expect("Invalid key")
            .expand(ExpansionMode::Ed25519);

        Ok(MiningContext {
            p3d_params,
            pool_id,
            member_id,
            key,
            cur_state: Mutex::new(None),
            in_queue: Mutex::new(VecDeque::new()),
            out_queue: Mutex::new(VecDeque::new()),
            client: HttpClientBuilder::default().build(pool_addr)?,
        })
    }

    pub(crate) fn on_new_object<C>(
        &self,
        params: Params<'_>,
        _ctx: &C,
    ) -> Result<JsonValue, Error> {
        let data: JsonValue = params.parse().unwrap();
        let obj_id = data.get(0).unwrap().as_u64().unwrap();
        let obj = data.get(1).unwrap().as_str().unwrap();
        let mining_obj = MiningObj {
            obj: obj.as_bytes().to_vec(),
            obj_id,
        };
        let mut lock = self.in_queue.lock().unwrap();
        (*lock).push_back(mining_obj);
        Ok(serde_json::json!(0))
    }

    pub(crate) fn push_to_queue(&self, prosal: MiningProposal) {
        let mut lock = self.out_queue.lock().unwrap();
        (*lock).push_back(prosal);
    }

    pub(crate) async fn ask_mining_params(&self) -> anyhow::Result<()> {
        println!("Ask mining params...");

        let response: JsonValue = self
            .client
            .request(
                "poscan_getMiningParams",
                rpc_params![serde_json::json!(self.pool_id)],
            )
            .await?;

        let pre_hash: Option<&str> = response.get(0).expect("Expect pre_hash").as_str();
        let parent_hash: Option<&str> = response.get(1).expect("Expect parent_hash").as_str();
        let win_dfclty: Option<&str> = response.get(2).expect("Expect win_difficulty").as_str();
        let pow_dfclty: Option<&str> = response.get(3).expect("Expect pow_difficulty").as_str();
        let pub_key: Option<&str> = response.get(4).expect("public key").as_str();

        match (pre_hash, parent_hash, win_dfclty, pow_dfclty, pub_key) {
            (
                Some(pre_hash),
                Some(parent_hash),
                Some(win_dfclty),
                Some(pow_dfclty),
                Some(pub_key),
            ) => {
                let pre_hash = H256::from_str(&pre_hash).unwrap();
                let parent_hash = H256::from_str(&parent_hash).unwrap();
                let win_dfclty = U256::from_str_radix(&win_dfclty, 16).unwrap();
                let pow_dfclty = U256::from_str_radix(&pow_dfclty, 16).unwrap();
                let pub_key = U256::from_str_radix(&pub_key, 16).unwrap();
                let mut pub_key = pub_key.encode();
                pub_key.reverse();
                let pub_key = ecies_ed25519::PublicKey::from_bytes(&pub_key).unwrap();

                let mut lock = self.cur_state.lock().unwrap();
                (*lock) = Some(MiningParams {
                    pre_hash,
                    parent_hash,
                    pow_dfclty,
                    win_dfclty,
                    pub_key,
                });
                println!("Mining params applied");
            }
            _ => {
                println!("Ask_mining_params error: Incorrect response from poll node");
            }
        }
        Ok(())
    }

    pub(crate) async fn push_to_node(&self, proposal: MiningProposal) -> anyhow::Result<()> {
        println!("Pushing obj to node...");

        let payload = Payload {
            pool_id:     self.pool_id.clone(),
            member_id:   self.member_id.clone(),
            pre_hash:    proposal.params.pre_hash,
            parent_hash: proposal.params.parent_hash,
            algo:        self.p3d_params.algo.as_str().into(),
            dfclty:      proposal.params.pow_dfclty,
            hash:        proposal.hash,
            obj_id:      proposal.obj_id,
            obj:         proposal.obj,
        };

        let message = serde_json::to_string(&payload).unwrap();
        let mut csprng = StdRng::from_seed(proposal.hash.encode().try_into().unwrap());
        let encrypted = encrypt(&proposal.params.pub_key, message.as_bytes(), &mut csprng).unwrap();
        let sign = self.sign(&encrypted);

        let params = rpc_params![
            serde_json::json!(encrypted.clone()),
            serde_json::json!(self.member_id.clone()),
            serde_json::json!(hex::encode(sign.to_bytes()))
        ];

        let _response: JsonValue = self
            .client
            .request("poscan_pushMiningObjectToPool", params)
            .await?;

        Ok(())
    }

    fn sign(&self, msg: &[u8]) -> Signature {
        const CTX: &[u8] = b"Mining pool";
        let sig = self.key.sign_simple(CTX, msg, &self.key.to_public());
        sig
    }
}

pub(crate) async fn run_server(ctx: Arc<MiningContext>) -> anyhow::Result<SocketAddr> {
    let server = ServerBuilder::default().build(LISTEN_ADDR).await?;
    let mut module = RpcModule::new(());
    let ctx = ctx.clone();
    module.register_method("poscan_pushMiningObject", move |p, c| {
        ctx.on_new_object(p, c)
    })?;
    let addr = server.local_addr()?;
    let handle = server.start(module)?;

    tokio::spawn(handle.stopped());

    println!("Server listening on {}", LISTEN_ADDR);

    Ok(addr)
}
