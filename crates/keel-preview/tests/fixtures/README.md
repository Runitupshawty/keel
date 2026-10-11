# Preview fixtures

Synthetic files only (no photos or recordings of anyone).

- `pattern.heic`, `pattern-rot90.heic`, `pattern.avif`: a 64 x 48 test pattern (green band
  along the top, red left half, blue right half) made with Pillow:

  ```python
  from PIL import Image
  im = Image.new("RGB", (64, 48))
  for y in range(48):
      for x in range(64):
          im.putpixel((x, y), (0, 200, 0) if y < 12 else ((220, 30, 30) if x < 32 else (30, 30, 220)))
  im.save("pattern.png")
  ```

  - `pattern.heic`: `pillow_heif` 1.5 (libheif, x265), `im.save("pattern.heic", quality=60)`.
  - `pattern-rot90.heic`: the same with EXIF orientation 6, which `pillow_heif` stores as
    an `irot` transform (a quarter turn clockwise): it displays 48 x 64 with the green band
    on the right.
  - `pattern.avif`: `ffmpeg -i pattern.png -c:v libaom-av1 -still-picture 1 -crf 40
    -pix_fmt yuv420p pattern.avif`.
