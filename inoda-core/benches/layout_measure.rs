use criterion::{Criterion, black_box, criterion_group, criterion_main};
use inoda_core::{css, html, layout};
use cosmic_text::FontSystem;
use std::collections::HashMap;

mod fixtures;

fn bench_layout(c: &mut Criterion) {
    let sheet = css::parse_stylesheet(fixtures::CSS_MEDIUM);
    c.bench_function("compute_layout_medium", |b| {
        b.iter(|| {
            let mut doc = html::parse_html(fixtures::HTML_MEDIUM);
            css::compute_styles(&mut doc, &sheet);
            let mut font_system = FontSystem::new();
            let mut buffer_cache = HashMap::new();
            let root = layout::compute_layout(
                black_box(&mut doc),
                320.0,
                480.0,
                black_box(&mut font_system),
                black_box(&mut buffer_cache),
            );
            black_box(root);
        });
    });
}

criterion_group!(benches, bench_layout);
criterion_main!(benches);
