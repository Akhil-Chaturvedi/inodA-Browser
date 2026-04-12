//! Noisy: QuickJS + DOM; use for local regression only.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use inoda_core::{html, js::JsEngine};

fn bench_js_get_by_id(c: &mut Criterion) {
    let html = "<html><body><p id=\"x\">hi</p></body></html>";
    c.bench_function("js_getElementById_loop", |b| {
        b.iter(|| {
            let doc = html::parse_html(html);
            let engine = JsEngine::try_new(doc).expect("try_new");
            for _ in 0..20 {
                let _ = black_box(
                    engine
                        .execute_script("document.getElementById('x').tagName")
                        .expect("execute_script"),
                );
            }
        });
    });
}

criterion_group!(benches, bench_js_get_by_id);
criterion_main!(benches);
