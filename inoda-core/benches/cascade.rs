use criterion::{Criterion, black_box, criterion_group, criterion_main};
use inoda_core::{css, html};

mod fixtures;

fn bench_cascade(c: &mut Criterion) {
    let sheet = css::parse_stylesheet(fixtures::CSS_MEDIUM);
    c.bench_function("compute_styles_medium", |b| {
        // iter_batched runs setup once, then measures iteration multiple times
        b.iter_batched(
            || html::parse_html(fixtures::HTML_MEDIUM),
            |mut doc| {
                css::compute_styles(black_box(&mut doc), black_box(&sheet));
                black_box(doc);
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, bench_cascade);
criterion_main!(benches);
