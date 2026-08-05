-- Strip image alt text / attributes so pandoc does not emit
-- \includegraphics[alt=...]. The alt key requires a recent graphicx
-- (TeX Live 2022+) and breaks PDF builds on older TeX distributions
-- with "Package keyval Error: alt undefined". Clearing the attributes
-- and caption keeps image rendering identical while staying portable
-- across pandoc and TeX versions.
function Image(img)
  img.attributes = {}
  img.caption = {}
  return img
end
