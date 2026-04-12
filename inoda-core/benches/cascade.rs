use criterion::{Criterion, black_box, criterion_group, criterion_main};
use inoda_core::{css, html};

mod fixtures;

fn bench_cascade(c: &mut Criterion) {
    let sheet = css::parse_stylesheet(fixtures::CSS_MEDIUM);
    c.bench_function("compute_styles_medium", |b| {
        b.iter(|| {
            let mut doc = html::parse_html(fixtures::HTML_MEDIUM);
            css::compute_styles(black_box(&mut doc), black_box(&sheet));
            black_box(doc);
        });
    });
}

criterion_group!(benches, bench_cascade);
criterion_main!(benches);
