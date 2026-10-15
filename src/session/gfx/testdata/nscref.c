/* Generate ClearCodec/NSCodec fixtures and reference BGR hashes with FreeRDP 3.31.
 *
 * cc nscref.c $(pkg-config --cflags --libs freerdp3 winpr3) -o /tmp/nscref
 * /tmp/nscref clearcodec-nsc.streams clearcodec-nsc.reference
 *
 * Records: tile width u16, height u16, stream length u32, ClearCodec stream (LE).
 * References: FNV-1a 64 over BGR bytes after decoding onto a 0x5A-filled tile.
 * No server recordings or production pixels are used.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <freerdp/codec/clear.h>
#include <freerdp/codec/nsc.h>

static void u16(FILE* f, UINT16 v) { fputc(v & 255, f); fputc(v >> 8, f); }
static void u32(FILE* f, UINT32 v) { u16(f, v & 65535); u16(f, v >> 16); }
static void u64(FILE* f, UINT64 v) { u32(f, v & 0xFFFFFFFF); u32(f, v >> 32); }

int main(int argc, char** argv)
{
	if (argc != 3) return 2;
	FILE* streams = fopen(argv[1], "wb");
	FILE* refs = fopen(argv[2], "wb");
	if (!streams || !refs) return 1;
	NSC_CONTEXT* nsc = nsc_context_new();
	CLEAR_CONTEXT* clear = clear_context_new(FALSE);
	if (!nsc || !clear || !nsc_context_set_parameters(nsc, NSC_COLOR_FORMAT, PIXEL_FORMAT_BGRA32))
		return 1;
	const UINT16 sizes[][2] = { {1,65}, {7,9}, {8,8}, {13,17}, {32,16}, {64,32}, {128,8} };
	UINT32 seed = 0x128934AB;
	unsigned count = 0;
	for (UINT32 loss = 1; loss <= 7; loss++)
	for (UINT32 sub = 0; sub <= 1; sub++)
	for (UINT32 pattern = 0; pattern < 4; pattern++)
	for (size_t size = 0; size < sizeof(sizes) / sizeof(sizes[0]); size++)
	{
		const UINT32 w = sizes[size][0], h = sizes[size][1];
		const UINT32 tw = w + 6, th = h + 4;
		BYTE* pixels = malloc(w * h * 4);
		for (UINT32 y = 0; y < h; y++)
		for (UINT32 x = 0; x < w; x++)
		{
			seed ^= seed << 13; seed ^= seed >> 17; seed ^= seed << 5;
			BYTE* p = pixels + (y * w + x) * 4;
			p[0] = pattern == 0 ? 80 : pattern == 1 ? (x / 4 % 2 ? 240 : 20) : pattern == 2 ? x * 13 : seed;
			p[1] = pattern == 0 ? 80 : pattern == 1 ? (y / 3 % 2 ? 10 : 200) : pattern == 2 ? y * 17 : seed >> 8;
			p[2] = pattern == 0 ? 80 : pattern == 1 ? 160 : pattern == 2 ? (x + y) * 7 : seed >> 16;
			p[3] = pattern < 2 ? 255 : seed >> 24;
		}
		wStream* encoded = Stream_New(NULL, w * h * 8 + 1024);
		if (!encoded || !nsc_context_set_parameters(nsc, NSC_COLOR_LOSS_LEVEL, loss) ||
		    !nsc_context_set_parameters(nsc, NSC_ALLOW_SUBSAMPLING, sub) ||
		    !nsc_compose_message(nsc, encoded, pixels, w, h, w * 4)) return 1;
		const UINT32 len = Stream_GetPosition(encoded);
		wStream* wire = Stream_New(NULL, len + 27);
		if (!wire) return 1;
		Stream_Write_UINT8(wire, 0); // flags
		Stream_Write_UINT8(wire, count % 256);
		Stream_Write_UINT32(wire, 0); // residual
		Stream_Write_UINT32(wire, 0); // bands
		Stream_Write_UINT32(wire, len + 13);
		Stream_Write_UINT16(wire, 2); // subcodec x
		Stream_Write_UINT16(wire, 1); // subcodec y
		Stream_Write_UINT16(wire, w);
		Stream_Write_UINT16(wire, h);
		Stream_Write_UINT32(wire, len);
		Stream_Write_UINT8(wire, 1); // NSCodec
		Stream_Write(wire, Stream_Buffer(encoded), len);
		const UINT32 wire_len = Stream_GetPosition(wire);
		BYTE* tile = malloc(tw * th * 4);
		memset(tile, 0x5A, tw * th * 4);
		if (clear_decompress(clear, Stream_Buffer(wire), wire_len, tw, th, tile,
		                     PIXEL_FORMAT_BGRA32, tw * 4, 0, 0, tw, th, NULL) < 0) return 1;
		UINT64 hash = 0xcbf29ce484222325ULL;
		for (size_t i = 0; i < tw * th * 4; i++)
			if (i % 4 != 3) hash = (hash ^ tile[i]) * 0x100000001b3ULL;
		u16(streams, tw); u16(streams, th); u32(streams, wire_len);
		fwrite(Stream_Buffer(wire), 1, wire_len, streams);
		u64(refs, hash);
		count++;
		free(tile); free(pixels);
		Stream_Free(wire, TRUE); Stream_Free(encoded, TRUE);
	}
	nsc_context_free(nsc); clear_context_free(clear);
	fclose(streams); fclose(refs);
	printf("%u NSCodec reference cases\n", count);
	return 0;
}
