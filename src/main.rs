use bussard::{bussard, dispatch_req, BussardMessage};
use pyo3::prelude::{PyResult, Python};
use pyo3::types::PyList;
use pyo3::{PyAny, PyTryInto};
use std::env;
use tokio::sync::mpsc;
use tokio::{task, join};
use warp::{filters::header::headers_cloned, Filter};
use futures::FutureExt;

fn add_paths(py: Python) -> PyResult<()> {
    let syspath: &PyList = py.import("sys")?.get("path")?.try_into()?;

    let mut cwd = env::current_dir().unwrap();
    cwd.push("src");
    syspath.insert(0, cwd.to_str())?;

    cwd.pop();
    cwd.push(".venv");
    cwd.push("Lib");
    cwd.push("site-packages");
    syspath.insert(1, cwd.to_str())?;

    Ok(())
}

fn flaskapp(py: Python) -> PyResult<&PyAny> {
    add_paths(py)?;

    let flask_main = py.import("main")?;
    let flask_app = flask_main.get("make_app")?.call0()?;

    Ok(flask_app)
}

fn with<T: Sized + Clone + Send>(
    t: T,
) -> impl Filter<Extract = (T,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || t.clone())
}

#[tokio::main]
async fn main() {
    let mut ch = mpsc::channel::<BussardMessage>(1024);
    let receiver = &mut ch.1;
    // We block in place so we don't have to send the python bits
    let bussard = task::block_in_place(move || bussard(receiver, flaskapp));

    let mut sender = ch.0;

    let bussarded = warp::any()
        .and(warp::path::tail())
        .and(headers_cloned())
        .and(warp::method())
        .and(warp::body::bytes())
        .and(with(sender.clone()))
        .and_then(dispatch_req);

    let server = warp::serve(bussarded).run(([127, 0, 0, 1], 3030)).then(|_f| async move {
        sender.send(BussardMessage::Shutdown).await.unwrap();
    });

    join!(server, bussard);
}
