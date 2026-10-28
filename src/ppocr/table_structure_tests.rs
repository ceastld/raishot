use super::*;

fn image(width: u32, height: u32, pixel: [u8; 4]) -> Image {
    Image {
        width,
        height,
        bgra: pixel.repeat((width * height) as usize),
        readback_ms: 0.,
    }
}

fn prediction(tokens: &[&str], bounds: &[[f32; 4]]) -> (Tensor, Tensor, Vec<String>) {
    let mut characters = vec!["sos".to_owned()];
    for token in tokens {
        if !characters.iter().any(|character| character == token) {
            characters.push((*token).into());
        }
    }
    characters.push("eos".into());
    let vocabulary = characters.len();
    let steps = tokens.len() + 1;
    let mut probabilities = Tensor {
        shape: vec![1, steps, vocabulary],
        values: vec![0.; steps * vocabulary],
    };
    let mut boxes = Tensor {
        shape: vec![1, steps, 8],
        values: vec![0.; steps * 8],
    };
    let mut cell = 0;
    for (sequence, token) in tokens.iter().enumerate() {
        let index = characters
            .iter()
            .position(|character| character == token)
            .unwrap();
        probabilities.values[sequence * vocabulary + index] = 0.9;
        if cell_start(token) {
            let [left, top, right, bottom] = bounds[cell];
            boxes.values[sequence * 8..sequence * 8 + 8]
                .copy_from_slice(&[left, top, right, top, right, bottom, left, bottom]);
            cell += 1;
        }
    }
    probabilities.values[tokens.len() * vocabulary + vocabulary - 1] = 1.;
    (probabilities, boxes, characters)
}

#[test]
fn table_structure_dictionary_follows_exported_token_order() {
    let characters =
        read_characters("<tbody>\r\n<tr>\r\n<td>\r\n</td>\r\n</tr>\r\n</tbody>\r\n").unwrap();
    assert_eq!(
        characters,
        [
            "sos",
            "<tbody>",
            "<tr>",
            "</td>",
            "</tr>",
            "</tbody>",
            "<td></td>",
            "eos"
        ]
    );
    let merged = read_characters("<tr>\n<td></td>\n</tr>").unwrap();
    assert_eq!(
        merged.iter().filter(|token| *token == "<td></td>").count(),
        1
    );
    assert!(read_characters("").is_err());
    assert!(read_characters("<tr>\n<tr>\n</tr>").is_err());
    assert!(read_characters("sos\n<tr>\n</tr>").is_err());
    assert!(read_characters("<tr>\n</tr>\n\0").is_err());
}

#[test]
fn table_structure_preprocess_bgr_normalization_and_padding() {
    let tensor = preprocess(&image(2, 1, [0, 128, 255, 255])).unwrap();
    let plane = INPUT_SIZE * INPUT_SIZE;
    assert_eq!(tensor.shape, [1, 3, 488, 488]);
    for offset in [0, 100 * INPUT_SIZE + 200, 243 * INPUT_SIZE + 487] {
        assert!((tensor.values[offset] - (0. - 0.485) / 0.229).abs() < 0.00001);
        assert!((tensor.values[plane + offset] - (128. / 255. - 0.456) / 0.224).abs() < 0.00001);
        assert!((tensor.values[2 * plane + offset] - (1. - 0.406) / 0.225).abs() < 0.00001);
    }
    for channel in 0..3 {
        assert!(
            tensor.values[channel * plane + 244 * INPUT_SIZE..(channel + 1) * plane]
                .iter()
                .all(|value| *value == 0.)
        );
    }
}

#[test]
fn table_structure_preprocess_flattens_alpha_and_samples_pixel_centers() {
    let transparent = preprocess(&image(1, 1, [1, 2, 3, 0])).unwrap();
    assert!((transparent.values[0] - (1. - 0.485) / 0.229).abs() < 0.00001);
    let mut source = image(2, 1, [0, 0, 0, 255]);
    source.bgra[4..8].copy_from_slice(&[255, 255, 255, 255]);
    let tensor = preprocess(&source).unwrap();
    assert!((tensor.values[0] - (0. - 0.485) / 0.229).abs() < 0.00001);
    assert!((tensor.values[487] - (1. - 0.485) / 0.229).abs() < 0.00001);
    assert!(tensor.values[244] > tensor.values[243]);
    let tall = preprocess(&image(1, 2, [0, 0, 0, 255])).unwrap();
    assert_eq!(tall.values[244], 0.);
    source.bgra.pop();
    assert!(preprocess(&source).is_err());
    assert!(preprocess(&image(0, 1, [0; 4])).is_err());
}

#[test]
fn table_structure_merges_rowspan_and_colspan_without_missing_covered_cells() {
    let tokens = [
        "<tbody>",
        "<tr>",
        "<td",
        " rowspan=\"2\"",
        ">",
        "</td>",
        "<td></td>",
        "<td></td>",
        "</tr>",
        "<tr>",
        "<td",
        " colspan=\"2\"",
        ">",
        "</td>",
        "</tr>",
        "</tbody>",
    ];
    let bounds = [
        [0., 0., 0.25, 0.5],
        [0.25, 0., 0.5, 0.25],
        [0.5, 0., 1., 0.25],
        [0.25, 0.25, 1., 0.5],
    ];
    let (probabilities, boxes, characters) = prediction(&tokens, &bounds);
    let grid = decode(&probabilities, &boxes, &characters, 200, 100).unwrap();
    assert_eq!(grid.column_edges, [0, 50, 100, 200]);
    assert_eq!(grid.row_edges, [0, 50, 100]);
    assert_eq!(grid.cells.len(), 4);
    assert_eq!((grid.cells[0].rowspan, grid.cells[0].colspan), (2, 1));
    assert_eq!(
        (
            grid.cells[3].row,
            grid.cells[3].column,
            grid.cells[3].colspan
        ),
        (1, 1, 2)
    );
    assert!(grid.cells.iter().all(|cell| cell.text.is_empty()));
    assert_eq!(grid.source, TableSource::Model);
    assert!(grid.validate());
}

#[test]
fn table_structure_boxes_use_longest_side_and_clamp_to_image() {
    let (probabilities, boxes, characters) =
        prediction(&["<tr>", "<td></td>", "</tr>"], &[[-0.1, 0.1, 1.1, 0.8]]);
    let grid = decode(&probabilities, &boxes, &characters, 200, 100).unwrap();
    assert_eq!(grid.column_edges, [0, 200]);
    assert_eq!(grid.row_edges, [20, 100]);
}

#[test]
fn table_structure_normalizes_adjacent_cell_gaps_and_hidden_span_edges() {
    let (probabilities, boxes, characters) = prediction(
        &["<tr>", "<td></td>", "<td></td>", "</tr>"],
        &[[0., 0., 0.48, 0.5], [0.52, 0., 1., 0.5]],
    );
    let grid = decode(&probabilities, &boxes, &characters, 100, 50).unwrap();
    assert_eq!(grid.column_edges, [0, 50, 100]);
    assert_eq!(grid.cells[0].rect.width, 50);
    let (probabilities, boxes, characters) = prediction(
        &["<tr>", "<td colspan='2'></td>", "</tr>"],
        &[[0., 0., 1., 0.5]],
    );
    let grid = decode(&probabilities, &boxes, &characters, 100, 50).unwrap();
    assert_eq!(grid.column_edges, [0, 50, 100]);
    assert_eq!(grid.cells[0].colspan, 2);
}

#[test]
fn table_structure_confidence_tracks_weakest_cell_instead_of_filler_tokens() {
    let (mut probabilities, boxes, characters) = prediction(
        &["<tr>", "<td></td>", "<td></td>", "</tr>"],
        &[[0., 0., 0.5, 0.5], [0.5, 0., 1., 0.5]],
    );
    let vocabulary = characters.len();
    let cell_index = characters
        .iter()
        .position(|token| token == "<td></td>")
        .unwrap();
    probabilities.values[vocabulary + cell_index] = 0.42;
    let row_index = characters.iter().position(|token| token == "<tr>").unwrap();
    probabilities.values[row_index] = 0.1;
    let grid = decode(&probabilities, &boxes, &characters, 100, 50).unwrap();
    assert_eq!(grid.confidence, 0.42);
    assert_eq!(grid.cells[0].confidence, 0.42);
    assert_eq!(grid.cells[1].confidence, 0.9);
}

#[test]
fn table_structure_rejects_truncation_even_when_last_row_looks_complete() {
    let (mut probabilities, mut boxes, characters) =
        prediction(&["<tr>", "<td></td>", "</tr>"], &[[0., 0., 1., 0.5]]);
    probabilities.shape[1] -= 1;
    probabilities
        .values
        .truncate(probabilities.shape[1] * characters.len());
    boxes.shape[1] -= 1;
    boxes.values.truncate(boxes.shape[1] * 8);
    assert!(decode(&probabilities, &boxes, &characters, 100, 50)
        .unwrap_err()
        .to_string()
        .contains("截断"));
}

#[test]
fn table_structure_rejects_invalid_shapes_numbers_and_geometry() {
    let (mut probabilities, mut boxes, characters) =
        prediction(&["<tr>", "<td></td>", "</tr>"], &[[0., 0., 1., 0.5]]);
    probabilities.shape[0] = 2;
    assert!(decode(&probabilities, &boxes, &characters, 100, 50).is_err());
    probabilities.shape[0] = 1;
    probabilities.values[0] = f32::NAN;
    assert!(decode(&probabilities, &boxes, &characters, 100, 50).is_err());
    probabilities.values[0] = 1.1;
    assert!(decode(&probabilities, &boxes, &characters, 100, 50).is_err());
    probabilities.values[0] = 0.;
    boxes.values.fill(0.);
    assert!(decode(&probabilities, &boxes, &characters, 100, 50).is_err());
    boxes.shape[2] = 4;
    assert!(decode(&probabilities, &boxes, &characters, 100, 50).is_err());
}

#[test]
fn table_structure_rejects_holes_overlaps_and_dangling_rowspans() {
    for (tokens, bounds) in [
        (
            vec![
                "<tr>",
                "<td></td>",
                "<td></td>",
                "</tr>",
                "<tr>",
                "<td></td>",
                "</tr>",
            ],
            vec![
                [0., 0., 0.5, 0.25],
                [0.5, 0., 1., 0.25],
                [0., 0.25, 0.5, 0.5],
            ],
        ),
        (
            vec!["<tr>", "<td rowspan=\"2\"></td>", "</tr>"],
            vec![[0., 0., 1., 0.5]],
        ),
        (
            vec![
                "<tr>",
                "<td></td>",
                "<td rowspan=\"2\"></td>",
                "</tr>",
                "<tr>",
                "<td colspan=\"2\"></td>",
                "</tr>",
            ],
            vec![[0., 0., 0.5, 0.25], [0.5, 0., 1., 0.5], [0., 0.25, 1., 0.5]],
        ),
    ] {
        let (probabilities, boxes, characters) = prediction(&tokens, &bounds);
        assert!(decode(&probabilities, &boxes, &characters, 100, 50).is_err());
    }
}

#[test]
fn table_structure_rejects_malformed_or_excessive_span_descriptors() {
    for descriptor in [
        "<td rowspan='0'></td>",
        "<td colspan='129'></td>",
        "<td rowspan='257'></td>",
        "<td rowspan='2' rowspan='2'></td>",
        "<td colspan='2></td>",
        "<td colspan='184467440737095516160'></td>",
        "<td onclick='2'></td>",
    ] {
        let (probabilities, boxes, characters) =
            prediction(&["<tr>", descriptor, "</tr>"], &[[0., 0., 1., 0.5]]);
        assert!(
            decode(&probabilities, &boxes, &characters, 100, 50).is_err(),
            "{descriptor}"
        );
    }
    let (probabilities, boxes, characters) = prediction(
        &["<tbody>", "<tr>", "<td></td>", "</tr>", "</thead>"],
        &[[0., 0., 1., 0.5]],
    );
    assert!(decode(&probabilities, &boxes, &characters, 100, 50).is_err());
}
