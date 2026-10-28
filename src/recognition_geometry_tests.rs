use super::*;

fn geometry(points: [[f32; 2]; 4]) -> TextGeometry {
    TextGeometry {
        quad: points.map(|[x, y]| Point { x, y }),
    }
}
fn near(actual: Point, expected: Point) {
    assert!(
        (actual.x - expected.x).abs() < 0.001,
        "x: {actual:?} != {expected:?}"
    );
    assert!(
        (actual.y - expected.y).abs() < 0.001,
        "y: {actual:?} != {expected:?}"
    );
}

#[test]
fn projected_directions_follow_reading_axis_in_all_four_orientations() {
    for (quad, direction) in [
        (
            [[10., 20.], [110., 20.], [110., 40.], [10., 40.]],
            ReadingDirection::LeftToRight,
        ),
        (
            [[110., 40.], [10., 40.], [10., 20.], [110., 20.]],
            ReadingDirection::RightToLeft,
        ),
        (
            [[30., 20.], [30., 120.], [10., 120.], [10., 20.]],
            ReadingDirection::TopToBottom,
        ),
        (
            [[10., 120.], [10., 20.], [30., 20.], [30., 120.]],
            ReadingDirection::BottomToTop,
        ),
    ] {
        let projection = Projection::new(geometry(quad)).unwrap();
        assert_eq!(projection.direction(), direction);
        for (i, [u, v]) in [[0., 0.], [1., 0.], [1., 1.], [0., 1.]]
            .into_iter()
            .enumerate()
        {
            near(
                projection.point(u, v),
                Point {
                    x: quad[i][0],
                    y: quad[i][1],
                },
            );
        }
    }
}

#[test]
fn projected_trapezoid_uses_perspective_instead_of_bilinear_center() {
    let projection =
        Projection::new(geometry([[10., 20.], [110., 20.], [90., 60.], [30., 60.]])).unwrap();
    // 此梯形独立解析式为 x=(100u+40v+10)/(1+2v/3), y=(80v+20)/(1+2v/3)。
    near(projection.point(0.5, 0.5), Point { x: 60., y: 45. });
    near(projection.point(0.25, 0.5), Point { x: 41.25, y: 45. });
    let local = projection.inverse(Point { x: 41.25, y: 45. }).unwrap();
    assert!((local[0] - 0.25).abs() < 1e-6);
    assert!((local[1] - 0.5).abs() < 1e-6);
}

#[test]
fn projected_slanted_affine_band_preserves_stops_and_cross_axis() {
    let projection =
        Projection::new(geometry([[10., 20.], [90., 80.], [78., 96.], [-2., 36.]])).unwrap();
    near(projection.point(0.25, 0.5), Point { x: 24., y: 43. });
    let local = projection.inverse(Point { x: 24., y: 43. }).unwrap();
    assert!((local[0] - 0.25).abs() < 1e-6);
    assert!((local[1] - 0.5).abs() < 1e-6);
    assert_eq!(projection.direction(), ReadingDirection::LeftToRight);
}

#[test]
fn projected_inverse_roundtrips_perspective_and_180_reading_coordinates() {
    for quad in [
        [[10., 20.], [110., 20.], [90., 60.], [30., 60.]],
        [[90., 60.], [30., 60.], [10., 20.], [110., 20.]],
    ] {
        let projection = Projection::new(geometry(quad)).unwrap();
        for u in [0., 0.1, 0.5, 0.9, 1.] {
            for v in [0., 0.25, 0.5, 1.] {
                let local = projection.inverse(projection.point(u, v)).unwrap();
                assert!((local[0] - u).abs() < 1e-6);
                assert!((local[1] - v).abs() < 1e-6);
            }
        }
    }
}

#[test]
fn projected_invalid_quad_never_creates_geometry() {
    for quad in [
        [[0., 0.], [0., 0.], [10., 10.], [0., 10.]],
        [[0., 0.], [10., 10.], [10., 0.], [0., 10.]],
        [[0., 0.], [0., 10.], [10., 10.], [10., 0.]],
        [[0., 0.], [10., 0.], [5., 0.], [0., 10.]],
        [[f32::NAN, 0.], [10., 0.], [10., 10.], [0., 10.]],
        [[0., 0.], [f32::INFINITY, 0.], [10., 10.], [0., 10.]],
        [
            [1_000_001., 0.],
            [1_000_011., 0.],
            [1_000_011., 10.],
            [1_000_001., 10.],
        ],
    ] {
        assert!(Projection::new(geometry(quad)).is_none());
    }
}

#[test]
fn projected_provider_band_requires_valid_containing_rect() {
    let band = geometry([[10., 20.], [110., 20.], [90., 60.], [30., 60.]]);
    assert!(band
        .validated_in(Rect {
            x: 10,
            y: 20,
            width: 100,
            height: 40
        })
        .is_some());
    for rect in [
        Rect {
            x: 10,
            y: 20,
            width: 0,
            height: 40,
        },
        Rect {
            x: 11,
            y: 20,
            width: 100,
            height: 40,
        },
        Rect {
            x: 10,
            y: 20,
            width: 100,
            height: 39,
        },
        Rect {
            x: u32::MAX,
            y: 20,
            width: 100,
            height: 40,
        },
    ] {
        assert!(band.validated_in(rect).is_none());
    }
}

#[test]
fn projected_roi_translation_preserves_reading_axis_with_negative_canvas_origin() {
    let original = geometry([[30., 20.], [30., 120.], [10., 120.], [10., 20.]]);
    let moved = TextGeometry {
        quad: original.quad.map(|p| Point {
            x: p.x - 500.,
            y: p.y + 70.,
        }),
    };
    let projection = Projection::new(moved).unwrap();
    near(projection.point(0.25, 0.5), Point { x: -480., y: 115. });
    assert_eq!(projection.direction(), ReadingDirection::TopToBottom);
    assert!(moved.validated_in(Rect::full(400, 400)).is_none());
}

#[test]
fn projected_inverse_rejects_nonfinite_pointer() {
    let projection =
        Projection::new(geometry([[0., 0.], [100., 0.], [100., 20.], [0., 20.]])).unwrap();
    assert!(projection
        .inverse(Point {
            x: f32::NAN,
            y: 10.
        })
        .is_none());
    assert!(projection
        .inverse(Point {
            x: 10.,
            y: f32::INFINITY
        })
        .is_none());
}
