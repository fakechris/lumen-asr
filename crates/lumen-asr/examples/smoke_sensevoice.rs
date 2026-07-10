use lumen_asr::{default_sensevoice_dir, sensevoice_ready, AsrEngine, AsrRequest, SenseVoiceSherpaAsr};

#[tokio::main]
async fn main() {
    let dir = default_sensevoice_dir();
    println!("dir={} ready={}", dir.display(), sensevoice_ready(&dir));
    if !sensevoice_ready(&dir) {
        println!("skip: model not ready");
        return;
    }
    let eng = SenseVoiceSherpaAsr::new(dir);
    let samples = vec![0.0f32; 16000];
    match eng
        .transcribe(AsrRequest {
            samples,
            sample_rate: 16000,
            hotwords: vec![],
        })
        .await
    {
        Ok(r) => println!("ok text={:?}", r.text),
        Err(e) => {
            eprintln!("err={e}");
            std::process::exit(1);
        }
    }
}
