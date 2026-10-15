/* Reference decodes for Relay's ClearCodec tests (FreeRDP 3.31).
 *
 *   clearref STREAMS OUT
 *     Decodes each record of STREAMS (width u16, height u16, length u32, data; little endian)
 *     in order with one ClearCodec context, into a tile pre-filled with 0x5A bytes (so pixels
 *     a stream doesn't draw stay recognisable), and writes one FNV-1a 64 hash of the tile's
 *     B, G, R bytes (alpha is not part of RDP surfaces) per record to OUT (0 when FreeRDP
 *     rejects the stream).
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <freerdp/codec/clear.h>

static UINT64 fnv1a(const BYTE* data, size_t len)
{
	UINT64 hash = 0xcbf29ce484222325ULL;
	for (size_t i = 0; i < len; i++)
		if (i % 4 != 3)
			hash = (hash ^ data[i]) * 0x100000001b3ULL;
	return hash;
}

int main(int argc, char** argv)
{
	if (argc < 3) { fprintf(stderr, "usage: clearref STREAMS OUT\n"); return 2; }
	FILE* in = fopen(argv[1], "rb");
	FILE* out = fopen(argv[2], "wb");
	if (!in || !out) { perror("open"); return 1; }
	CLEAR_CONTEXT* clear = clear_context_new(FALSE);
	BYTE header[8];
	int n = 0, failed = 0;
	while (fread(header, 1, 8, in) == 8)
	{
		const UINT32 w = header[0] | header[1] << 8, h = header[2] | header[3] << 8;
		const UINT32 len = header[4] | header[5] << 8 | header[6] << 16 | (UINT32)header[7] << 24;
		BYTE* data = malloc(len);
		if (fread(data, 1, len, in) != len) return 1;
		BYTE* tile = malloc(w * h * 4);
		memset(tile, 0x5A, w * h * 4);
		INT32 rc = clear_decompress(clear, data, len, w, h, tile, PIXEL_FORMAT_BGRA32, w * 4, 0, 0,
		                            w, h, NULL);
		UINT64 hash = rc < 0 ? 0 : fnv1a(tile, w * h * 4);

		if (rc < 0) { fprintf(stderr, "stream %d rejected (%d)\n", n, rc); failed++; }
		fwrite(&hash, 8, 1, out);
		free(data);
		free(tile);
		n++;
	}
	fclose(out);
	printf("%d streams, %d rejected\n", n, failed);
	return 0;
}
