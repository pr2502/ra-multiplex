use anyhow::{bail, Context, Result};
use ra_multiplex::config::Config;
use ra_multiplex::proto;
use serde_json::{Map, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::{io, task};

pub async fn main(server_path: String, server_args: Vec<String>) -> Result<()> {
    let config = Config::load_or_default().await;

    let stream = TcpStream::connect(config.connect)
        .await
        .context("connect")?;
    let (mut read_stream, mut write_stream) = stream.into_split();

    {
        let mut buf = Vec::new();
        let mut r = tokio::io::BufReader::new(read_stream);
        let (mut json, _) = ra_multiplex::lsp::read_message(&mut r, &mut buf)
            .await?
            .context("received invalid InitializeRequest")?;

        let proto_init = proto::Init::new(server_path, server_args);
        inject_options_into_initialization_request(&mut json, proto_init)?;

        write_stream.write_all(&serde_json::to_vec(&json)?).await?;

        // Revert read_stream to its original state.
        read_stream = r.into_inner();
    }

    let t1 = task::spawn(async move {
        io::copy(&mut read_stream, &mut io::stdout())
            .await
            .context("io error")
    });
    let t2 = task::spawn(async move {
        io::copy(&mut io::stdin(), &mut write_stream)
            .await
            .context("io error")
    });
    tokio::select! {
        res = t1 => res,
        res = t2 => res,
    }
    .context("join")??;
    Ok(())
}

fn inject_options_into_initialization_request(
    request: &mut Map<String, Value>,
    opts: proto::Init,
) -> anyhow::Result<()> {
    if !matches!(request.get("method"), Some(Value::String(method)) if method == "initialize") {
        bail!("first client message was not InitializeRequest");
    }

    let params = request
        .get_mut("params")
        .context("initialization request should contain params")?
        .as_object_mut()
        .context("initialization request params must be an object")?;

    let init_options = match params.get_mut("initializationOptions") {
        Some(opts) => opts
            .as_object_mut()
            // Technically it can be anything. I don't think any LSP uses a non-object here.
            .context("initializationOptions should be an object")?,
        None => {
            let opts = Map::new();
            params.insert(String::from("initializationOptions"), Value::from(opts));
            params
                .get_mut("initializationOptions")
                .expect("initializationOptions must be present")
                .as_object_mut()
                .expect("initializationOptions must be an object")
        }
    };

    let (cwd, server, args) = (opts.cwd, opts.server, opts.args);

    let multiplex_opts = {
        let mut m = Map::new();
        m.insert(String::from("cwd"), Value::from(cwd));
        m.insert(String::from("server"), Value::from(server));
        m.insert(String::from("args"), Value::from(args));
        m
    };
    init_options.insert(String::from("raMultiplex"), Value::from(multiplex_opts));

    Ok(())
}
