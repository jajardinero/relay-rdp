/* Reference data for Relay's progressive decoder tests (FreeRDP 3.31).
 *
 *   progref DIR W H
 *     Encodes a test picture with FreeRDP's encoder and decodes it with FreeRDP's decoder:
 *     DIR/{source,reference}.rgba and DIR/stream.bin.
 *   progref --decode W H OUT FRAME...
 *     Decodes progressive frames in order on one surface, writing OUT.N.rgba after frame N.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <freerdp/codec/progressive.h>
#include <freerdp/codec/region.h>

static void save(const char* dir, const char* name, const void* data, size_t len)
{
	char path[1024];
	snprintf(path, sizeof(path), "%s/%s", dir, name);
	FILE* f = fopen(path, "wb");
	if (!f || fwrite(data, 1, len, f) != len) { perror(path); exit(1); }
	fclose(f);
}

static int decode_frames(int argc, char** argv)
{
	const UINT32 w = (UINT32)atoi(argv[2]), h = (UINT32)atoi(argv[3]);
	PROGRESSIVE_CONTEXT* dec = progressive_context_new(FALSE);
	if (progressive_create_surface_context(dec, 1, w, h) < 0) return 1;
	BYTE* out = calloc(w * h, 4);
	for (int i = 5; i < argc; i++)
	{
		FILE* f = fopen(argv[i], "rb");
		if (!f) { perror(argv[i]); return 1; }
		static BYTE data[1 << 20];
		size_t size = fread(data, 1, sizeof(data), f);
		fclose(f);
		REGION16 invalid;
		region16_init(&invalid);
		INT32 rc = progressive_decompress(dec, data, (UINT32)size, out, PIXEL_FORMAT_RGBA32, w * 4,
		                                  0, 0, &invalid, 1, (UINT32)(i - 5));
		if (rc < 0) { fprintf(stderr, "%s: decompress failed %d\n", argv[i], rc); return 1; }
		char name[256];
		snprintf(name, sizeof(name), "%s.%d.rgba", argv[4], i - 4);
		save(".", name, out, w * h * 4);
	}
	return 0;
}

int main(int argc, char** argv)
{
	if (argc > 5 && strcmp(argv[1], "--decode") == 0)
		return decode_frames(argc, argv);
	if (argc < 4) { fprintf(stderr, "usage: progref DIR W H\n"); return 2; }
	const UINT32 w = (UINT32)atoi(argv[2]), h = (UINT32)atoi(argv[3]);
	BYTE* src = calloc(w * h, 4);
	UINT32 seed = 0x2545f491;
	for (UINT32 y = 0; y < h; y++)
		for (UINT32 x = 0; x < w; x++)
		{
			BYTE* p = &src[(y * w + x) * 4];
			seed ^= seed << 13; seed ^= seed >> 17; seed ^= seed << 5;
			/* Smooth colour gradients, a sharp-edged box, and mild noise. */
			int r = (int)(x * 255 / w), g = (int)(y * 255 / h), b = (int)((x + y) * 127 / (w + h)) + 64;
			if (x > w / 4 && x < w / 2 && y > h / 3 && y < 2 * h / 3) { r = 230; g = 40; b = 30; }
			int n = (int)(seed >> 29) - 4;
			p[0] = (BYTE)(r + n < 0 ? 0 : r + n > 255 ? 255 : r + n);
			p[1] = (BYTE)(g + n < 0 ? 0 : g + n > 255 ? 255 : g + n);
			p[2] = (BYTE)(b + n < 0 ? 0 : b + n > 255 ? 255 : b + n);
			p[3] = 0xFF;
		}
	save(argv[1], "source.rgba", src, w * h * 4);

	PROGRESSIVE_CONTEXT* enc = progressive_context_new(TRUE);
	REGION16 region;
	region16_init(&region);
	RECTANGLE_16 all = { 0, 0, (UINT16)w, (UINT16)h };
	if (!region16_union_rect(&region, &region, &all)) return 1;
	BYTE* stream = NULL;
	UINT32 size = 0;
	if (progressive_compress(enc, src, w * h * 4, PIXEL_FORMAT_RGBA32, w, h, w * 4, &region,
	                         &stream, &size) < 0)
	{ fprintf(stderr, "compress failed\n"); return 1; }
	save(argv[1], "stream.bin", stream, size);

	PROGRESSIVE_CONTEXT* dec = progressive_context_new(FALSE);
	if (progressive_create_surface_context(dec, 1, w, h) < 0) return 1;
	BYTE* out = calloc(w * h, 4);
	REGION16 invalid;
	region16_init(&invalid);
	INT32 rc = progressive_decompress(dec, stream, size, out, PIXEL_FORMAT_RGBA32, w * 4, 0, 0,
	                                  &invalid, 1, 0);
	if (rc < 0) { fprintf(stderr, "decompress failed %d\n", rc); return 1; }
	save(argv[1], "reference.rgba", out, w * h * 4);
	printf("%ux%u: %u bytes\n", w, h, size);
	return 0;
}
