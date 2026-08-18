import pydicom, glob, json, os, collections

DATA = r'E:/Codes/Unixel/data/CBCT'
OUT = r'E:/Codes/Unixel/data/cbct_scan_report.json'
# 压缩传输语法前缀（JPEG 系列 1.2.840.10008.1.2.4.*，RLE 1.2.840.10008.1.2.5）
COMPRESS_PREFIXES = ('1.2.840.10008.1.2.4', '1.2.840.10008.1.2.5')

files = sorted(glob.glob(os.path.join(DATA, '*.dcm')))
stats = {
    'total': len(files), 'ok': 0, 'fail': 0,
    'transfer_syntax': collections.Counter(),
    'rows_cols': collections.Counter(),
    'samples': collections.Counter(),
    'photometric': collections.Counter(),
    'bits_allocated': collections.Counter(),
    'bits_stored': collections.Counter(),
    'pixel_repr': collections.Counter(),
    'modalities': collections.Counter(),
    'compressed': 0,
    'multisample': 0,
    'has_rescale': 0,
    'errors': [],
}
wc_list, ww_list = [], []
slope_set, intercept_set = set(), set()


def first_float(v):
    if v is None:
        return None
    try:
        if isinstance(v, (list, pydicom.multival.MultiValue)):
            v = v[0]
        return float(v)
    except Exception:
        return None


for f in files:
    try:
        ds = pydicom.dcmread(f, force=True, stop_before_pixels=True)
        stats['ok'] += 1
        meta = getattr(ds, 'file_meta', None)
        if meta is not None and 'TransferSyntaxUID' in meta:
            ts = meta.TransferSyntaxUID
            ts_str = str(ts)
            name = getattr(ts, 'name', ts_str) or ts_str
            stats['transfer_syntax'][f'{ts_str} ({name})'] += 1
            if ts_str.startswith(COMPRESS_PREFIXES):
                stats['compressed'] += 1
        rows = int(getattr(ds, 'Rows', 0) or 0)
        cols = int(getattr(ds, 'Columns', 0) or 0)
        stats['rows_cols'][f'{rows}x{cols}'] += 1
        spp = int(getattr(ds, 'SamplesPerPixel', 0) or 0)
        stats['samples'][spp] += 1
        if spp != 1:
            stats['multisample'] += 1
        stats['photometric'][str(getattr(ds, 'PhotometricInterpretation', ''))] += 1
        stats['bits_allocated'][int(getattr(ds, 'BitsAllocated', 0) or 0)] += 1
        stats['bits_stored'][int(getattr(ds, 'BitsStored', 0) or 0)] += 1
        stats['pixel_repr'][int(getattr(ds, 'PixelRepresentation', 0) or 0)] += 1
        stats['modalities'][str(getattr(ds, 'Modality', ''))] += 1
        slope = getattr(ds, 'RescaleSlope', None)
        intercept = getattr(ds, 'RescaleIntercept', None)
        if slope is not None or intercept is not None:
            stats['has_rescale'] += 1
            slope_set.add(str(slope))
            intercept_set.add(str(intercept))
        wc = first_float(getattr(ds, 'WindowCenter', None))
        ww = first_float(getattr(ds, 'WindowWidth', None))
        if wc is not None:
            wc_list.append(wc)
        if ww is not None:
            ww_list.append(ww)
    except Exception as e:
        stats['fail'] += 1
        if len(stats['errors']) < 20:
            stats['errors'].append({'file': os.path.basename(f), 'error': str(e)[:300]})


def summ(lst):
    return None if not lst else {'min': min(lst), 'max': max(lst), 'n': len(lst)}


report = {
    'stats': {k: (dict(v) if isinstance(v, collections.Counter) else v) for k, v in stats.items()},
    'window_center': summ(wc_list),
    'window_width': summ(ww_list),
    'rescale_slope_values': sorted(slope_set),
    'rescale_intercept_values': sorted(intercept_set),
}
with open(OUT, 'w', encoding='utf-8') as fp:
    json.dump(report, fp, indent=2, ensure_ascii=False)
print(json.dumps(report, indent=2, ensure_ascii=False))
