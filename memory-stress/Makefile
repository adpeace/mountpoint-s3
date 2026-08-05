# Build the memory-limiter stress test findings report to PDF.
#
# Uses xelatex for native UTF-8 (the doc contains en/em dashes and box-like
# glyphs) with TeX Gyre Pagella for the body (a Palatino clone) and DejaVu
# Sans Mono for code. The Lua filter strips image alt attributes so the
# build stays portable across older TeX distributions (graphicx without the
# `alt` key). -implicit_figures keeps images inline at their reference point
# rather than floating them to separate pages.
#
# Requires: pandoc, a TeX install providing xelatex, and the TeX Gyre Pagella
# and DejaVu Sans Mono fonts.

SRC := RESULTS.md
PDF := $(SRC:.md=.pdf)
IMAGES := $(wildcard runs/*/rss.png runs/*/pool.png)

PANDOC := pandoc \
	-V geometry:margin=2cm \
	--pdf-engine=xelatex \
	-V mainfont="TeX Gyre Pagella" \
	-V monofont="DejaVu Sans Mono" \
	--lua-filter=strip-image-alt.lua \
	--from=markdown-implicit_figures

all: $(PDF)

$(PDF): $(SRC) $(IMAGES) strip-image-alt.lua
	$(PANDOC) $< -o $@

clean:
	rm -f $(PDF)

.PHONY: all clean
