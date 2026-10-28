use super::*;

#[test]
fn formula_layout_renders_math_extracted_from_mixed_user_source() {
    let math = r"n \in \mathbb{N}, \quad a_n = \frac{(-1)^{n+1}}{2n-1}";
    let image = render(math, false, 28.0).unwrap();
    assert!(image.width > 100 && image.height > 30);
    assert!(image.bgra.chunks_exact(4).any(|pixel| pixel[0] < 128));
    // 某一块的标签关联错误仍须留在该块，不能为继续显示其它块而放宽数学校验。
    assert!(render(r"\underbrace{\hspace{6em}}_{n}", false, 28.0).is_err());
}

#[test]
fn formula_layout_renders_real_math_and_theme_without_blank_pixels() {
    let light = render(r"\frac{1}{2}+\sqrt{x^2+1}", false, 32.0).unwrap();
    let dark = render(r"\frac{1}{2}+\sqrt{x^2+1}", true, 32.0).unwrap();
    assert_eq!((light.width, light.height), (dark.width, dark.height));
    assert_eq!(
        light.bgra.len(),
        light.width as usize * light.height as usize * 4
    );
    assert!(light.width > 50 && light.height > 30);
    assert_eq!(&light.bgra[..4], &[255, 255, 255, 255]);
    assert!(dark.bgra[0] < 60 && dark.bgra[1] < 60 && dark.bgra[2] < 60);
    assert!(light
        .bgra
        .chunks_exact(4)
        .any(|pixel| pixel[0] < 128 && pixel[1] < 128 && pixel[2] < 128));
    assert!(dark
        .bgra
        .chunks_exact(4)
        .any(|pixel| pixel[0] > 180 && pixel[1] > 180 && pixel[2] > 180));
}

#[test]
fn formula_layout_limits_dimensions_before_allocating_png() {
    assert!(checked_dimensions(f64::INFINITY, 1.0, 32.0).is_err());
    assert!(checked_dimensions(1_000_000.0, 1.0, 32.0).is_err());
    assert!(checked_dimensions(200.0, 200.0, 32.0).is_err());
    assert!(render(r"\rule{100000em}{1em}", false, 32.0).is_err());
    assert!(render("x", false, f32::NAN).is_err());
    assert!(render(r"\underbrace{\hspace{5em}}_n", false, 32.0).is_err());
}

#[test]
fn formula_layout_chinese_is_drawn_or_explicitly_rejected() {
    let source = r"当 x>0 时，f(x)=x";
    let characters = source
        .chars()
        .filter(|ch| crate::formula::is_cjk(*ch))
        .collect();
    match chinese_metrics(&characters) {
        Ok(_) => {
            let image = render(source, true, 32.0).unwrap();
            assert!(image.width > 100);
            assert!(image.bgra.chunks_exact(4).any(|pixel| pixel[0] > 180));
        }
        Err(error) => assert_eq!(render(source, true, 32.0).err().unwrap(), error),
    }
}

#[test]
fn formula_layout_common_structures_have_real_ink_and_multiline_height() {
    let expressions = [
        r"\sum_{k=1}^{n}k=\frac{n(n+1)}{2}",
        r"\int_{0}^{\infty}e^{-x}\,dx=1",
        r"\begin{pmatrix}a&b\\c&d\end{pmatrix}",
        r"f(x)=\begin{cases}x^2&x\ge0\\-x&x<0\end{cases}",
        r"\begin{aligned}a+b&=c\\x&=\frac{y}{z}\end{aligned}",
        r"\underbrace{a_1+\cdots+a_n}_{n}",
    ];
    let single = render("x", false, 32.0).unwrap();
    for (index, expression) in expressions.into_iter().enumerate() {
        let image = render(expression, false, 32.0)
            .unwrap_or_else(|error| panic!("common formula {index} failed: {error}"));
        assert!(image.width > 24 && image.height > 24);
        assert!(image.width <= MAX_SIDE && image.height <= MAX_SIDE);
        assert!(
            image
                .bgra
                .chunks_exact(4)
                .any(|pixel| { pixel[0] < 128 && pixel[1] < 128 && pixel[2] < 128 }),
            "common formula {index} rendered only its background"
        );
        if matches!(index, 2..=4) {
            assert!(
                image.height > single.height,
                "multiline structure {index} lost its rows"
            );
        }
    }
    let mixed = r"\text{当 }x>0\text{ 时，}f(x)=x";
    let characters = mixed
        .chars()
        .filter(|ch| crate::formula::is_cjk(*ch))
        .collect();
    match chinese_metrics(&characters) {
        Ok(_) => {
            let image = render(mixed, false, 32.0).unwrap();
            assert!(image.bgra.chunks_exact(4).any(|pixel| pixel[0] < 128));
        }
        Err(error) => assert_eq!(render(mixed, false, 32.0).err().unwrap(), error),
    }
}
