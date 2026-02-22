# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "opencv-python-headless",
#     "numpy",
#     "cairosvg",
#     "scipy",
# ]
# ///
import cv2
import numpy as np
import argparse
from scipy.interpolate import splprep, splev


def optimize_svg(src_path, dest_svg, epsilon=0.5, stroke_width=1.5, smoothing_s=100.0):
    img_orig = cv2.imread(src_path, cv2.IMREAD_UNCHANGED)
    h, w = img_orig.shape[:2]

    if img_orig.shape[2] == 4:
        alpha = img_orig[:, :, 3]
        opaque_mask = alpha > 200
        rgb = img_orig[:, :, :3]
        Z = rgb[opaque_mask]
    else:
        alpha = np.ones((h, w), dtype=np.uint8) * 255
        opaque_mask = alpha > 0
        Z = img_orig[:, :, :3].reshape((-1, 3))
        rgb = img_orig[:, :, :3]

    blurred = cv2.GaussianBlur(rgb, (3, 3), 0)
    Z_blur = blurred[opaque_mask]

    K_colors = 10
    Z_float = np.float32(Z_blur)
    criteria = (cv2.TERM_CRITERIA_EPS + cv2.TERM_CRITERIA_MAX_ITER, 200, 0.1)
    ret, label, center = cv2.kmeans(
        Z_float, K_colors, None, criteria, 10, cv2.KMEANS_PP_CENTERS
    )
    center = np.uint8(center)

    valid_mask = alpha > 10
    valid_float = np.float32(blurred[valid_mask])

    distances = np.linalg.norm(
        valid_float[:, None, :] - np.float32(center)[None, :, :], axis=2
    )
    labels_valid = np.argmin(distances, axis=1)

    label_img = np.zeros((h, w), dtype=np.int32) - 1
    label_img[valid_mask] = labels_valid

    sorted_indices = np.argsort(np.sum(center, axis=1))

    svg_output = []
    svg_output.append(
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{w:g}" height="{h:g}" viewBox="0 0 {w:g} {h:g}">'
    )

    valid_mask_uint8 = np.uint8(valid_mask) * 255

    for i in sorted_indices:
        c = center[i]

        if np.all(c > 240):
            continue

        mask = np.uint8(label_img == i) * 255
        mask = cv2.dilate(mask, np.ones((3, 3), np.uint8), iterations=1)
        mask = cv2.bitwise_and(mask, valid_mask_uint8)
        mask = cv2.morphologyEx(mask, cv2.MORPH_CLOSE, np.ones((5, 5), np.uint8))

        contours, hierarchy = cv2.findContours(
            mask, cv2.RETR_TREE, cv2.CHAIN_APPROX_TC89_L1
        )
        hex_c = "#%02x%02x%02x" % (c[2], c[1], c[0])

        path_data = []
        for cnt in contours:
            if cv2.contourArea(cnt) < 50:
                continue

            approx = cv2.approxPolyDP(cnt, epsilon, True)
            pts = approx.reshape(-1, 2)

            if len(pts) >= 4 and smoothing_s > 0:
                try:
                    tck, u = splprep([pts[:, 0], pts[:, 1]], s=smoothing_s, per=True)
                    u_new = np.linspace(0, 1.0, len(pts) * 5)
                    x_new, y_new = splev(u_new, tck)
                    x_new = np.clip(x_new, 0, w)
                    y_new = np.clip(y_new, 0, h)
                    pts = np.column_stack((x_new, y_new))
                except Exception as e:
                    pass

            pts = np.round(pts, 1)

            if len(pts) < 3:
                continue

            subpath = []
            for j, p in enumerate(pts):
                cmd = "M" if j == 0 else "L"
                subpath.append(f"{cmd}{p[0]:g},{p[1]:g}")
            subpath.append("Z")
            path_data.append(" ".join(subpath))

        if path_data:
            d = " ".join(path_data)
            svg_output.append(
                f'  <path d="{d}" fill="{hex_c}" stroke="{hex_c}" stroke-width="{stroke_width:g}" stroke-linejoin="round" stroke-linecap="round" fill-rule="evenodd" />'
            )

    svg_output.append("</svg>")

    with open(dest_svg, "w") as f:
        f.write("\n".join(svg_output))

    print(f"Cleaned up SVG saved to {dest_svg}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--input", default="logo.png")
    parser.add_argument("--output", default="logo.svg")
    args = parser.parse_args()

    optimize_svg(
        args.input, args.output, epsilon=0.5, stroke_width=1.5, smoothing_s=100.0
    )
