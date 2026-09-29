"""Dump PyMuPDF Layout's intermediate values for selected pages: the reference for
the Rust port, checked with `cargo run --release -p strata-core --example layout_golden <dir>`.

Needs `pip install pymupdf-layout` (the version in vendor/pymupdf_layout/README.md).

    python layout_golden.py <out dir> <file.pdf>:<page>[,<page>...] [...]

Pages count from 0. Output: <out dir>/<pdf stem>_p<page>.json
"""
import json, os, sys
import numpy as np
import pymupdf
from pymupdf.layout.onnx.BoxRFDGNN import BoxRFDGNN, get_nn_input_from_datadict
from pymupdf.layout.pymupdf_util import create_input_data_from_page
from pymupdf.layout.common_util import resize_image, to_gray, get_edge_matrix, group_node_by_edge

if len(sys.argv) < 3:
    sys.exit(__doc__)
OUT = sys.argv[1]
PAGES = []
for arg in sys.argv[2:]:
    path, _, pages = arg.rpartition(':')
    PAGES.append((path, [int(p) for p in pages.split(',')]))
os.makedirs(OUT, exist_ok=True)

m = BoxRFDGNN(feature_set_name='imf+rf', use_sort=False)


def r(a, n=6):
    return np.round(np.asarray(a, dtype=np.float64), n).tolist()


for path, pages in PAGES:
    doc = pymupdf.open(path)
    for pno in pages:
        page = doc[pno]
        dd = create_input_data_from_page(page, options={'input_type': m.input_type, 'feature_set_name': m.feature_set_name, 'feature_extractor': m.feature_extractor})
        # preprocessing of the CNN input, recomputed exactly as ImageFeatureExtractorV1.predict
        img = dd['image']
        g = to_gray(resize_image(img, (300, 300))).astype(np.float32)
        lo, hi = g.min(), g.max()
        g = (g - lo) / (hi - lo) if hi > lo else np.zeros_like(g)
        x, ei, ea, _, _, rf, tp, imf, _ = get_nn_input_from_datadict(dd, m.cfg)
        inputs = {'x': x, 'edge_index': ei, 'edge_attr': ea, 'rf_features': rf, 'k': np.array(min(len(x), 20), dtype=np.int64),
                  'text_patterns': tp, 'image_features': imf, 'batch': np.zeros(len(x), dtype=np.int64)}
        nl, el = m.session.run(None, {k: inputs[k] for k in m._onnx_input_names})
        p = np.exp(nl - nl.max(1, keepdims=True)); p /= p.sum(1, keepdims=True)
        lab = p.argmax(1)
        ep = np.exp(el - el.max(1, keepdims=True)); ep /= ep.sum(1, keepdims=True)
        elab = (ep[:, 1] > 0.55).astype(np.int64)
        groups = group_node_by_edge(lab, p[np.arange(len(lab)), lab], get_edge_matrix(len(lab), ei, elab), dd['bboxes'], m.class_priority_list)
        rec = {
            'page_size': [float(page.rect.width), float(page.rect.height)],
            'image_shape': list(img.shape),
            'gray_sum': float(g.sum()), 'gray_head': r(g.ravel()[:20]),
            'combined_mean': float(dd['feature_map'].mean()), 'logits_mean': float(dd['class_logits'].mean()),
            'bboxes': r(dd['bboxes'], 4), 'text': dd['text'],
            'rf_names': m.cfg['data']['rf_names'],
            'x': r(x), 'edge_index': np.asarray(ei).T.tolist(), 'edge_attr': r(ea), 'rf': r(rf), 'text_patterns': r(tp), 'image_features': r(imf),
            'node_logits': r(nl), 'edge_logits': r(el), 'labels': [m.data_class_names[i] for i in lab],
            'groups': [{'bbox': r(gr['group_bbox'], 3), 'class': m.data_class_names[gr['group_class']], 'nodes': gr['indicies']} for gr in groups],
        }
        name = f"{os.path.splitext(os.path.basename(path))[0]}_p{pno + 1}.json"
        with open(os.path.join(OUT, name), 'w', encoding='utf-8') as f:
            json.dump(rec, f, ensure_ascii=False)
        print(name, len(x), 'nodes', len(np.asarray(ei).T), 'edges', len(groups), 'groups')
