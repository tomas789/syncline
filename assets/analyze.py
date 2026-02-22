import cv2
import numpy as np
import collections

img = cv2.imread("logo.png", cv2.IMREAD_UNCHANGED)
h, w, c = img.shape
print(f"Shape: {w}x{h} with {c} channels")

if c == 4:
    # Blend with white bg
    alpha = img[:, :, 3] / 255.0
    bg = np.ones_like(img[:, :, :3]) * 255
    res = (
        img[:, :, :3] * alpha[:, :, np.newaxis] + bg * (1 - alpha[:, :, np.newaxis])
    ).astype(np.uint8)
else:
    res = img

# Sample a horizontal line across the middle (y = h // 2)
# and print colors to see thickness and progression.
y = h // 2
line = res[y, :]
# find unique contiguous color ranges
out = []
last_color = None
count = 0
for x in range(w):
    color = tuple(int(x) for x in line[x])  # BGR
    if last_color is None:
        last_color = color
        count = 1
    elif np.linalg.norm(np.array(color) - np.array(last_color)) < 10:
        count += 1
    else:
        out.append((last_color, count, x - count, x - 1))
        last_color = color
        count = 1
out.append((last_color, count, w - count, w - 1))

for c, count, x1, x2 in out:
    if count > 5:  # ignore 1-2 pixel anti-aliasing
        # BGR to RGB hex
        hex_c = "#%02x%02x%02x" % (c[2], c[1], c[0])
        print(f"At {x1}-{x2} ({count}px): {hex_c} RGB={c}")

# Find prominent colors in whole image
flat = res.reshape(-1, 3)
# simple quantization to 16 bits to find top colors
q = (flat // 16) * 16
unique_colors, counts = np.unique(q, axis=0, return_counts=True)
sorted_idx = np.argsort(-counts)

print("Top 10 prominent colors (quantized):")
for i in range(10):
    c = unique_colors[sorted_idx[i]]
    count = counts[sorted_idx[i]]
    if count > 1000:
        hex_c = "#%02x%02x%02x" % (c[2], c[1], c[0])
        print(f"{hex_c} : {count} pixels")
