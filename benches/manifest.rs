use air_transcode::hls::{TrackKind, media_playlist, segment_map};
use criterion::{Criterion, criterion_group, criterion_main};

fn render_two_hour_manifest(criterion: &mut Criterion) {
    let segments = segment_map(2 * 60 * 60 * 1_000_000_000, 4_000_000_000);
    criterion.bench_function("render two-hour VOD playlist", |bencher| {
        bencher.iter(|| media_playlist(TrackKind::Video, std::hint::black_box(&segments)));
    });
}

criterion_group!(benches, render_two_hour_manifest);
criterion_main!(benches);
