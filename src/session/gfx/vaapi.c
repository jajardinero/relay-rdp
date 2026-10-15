/* The only FFmpeg ABI boundary. Compile against system headers, never mirror
 * FFmpeg structs in Rust. No decoded pixels are mapped or downloaded here. */
#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_vaapi.h>
#include <va/va_drmcommon.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

struct relay_object { int fd; uint32_t size; uint64_t modifier; };
struct relay_layer {
    uint32_t format, num_planes;
    uint32_t object_index[4], offset[4], pitch[4];
};
struct relay_desc {
    uint32_t width, height, allocation_width, allocation_height;
    int colorspace, full_range, chroma_location;
    uint32_t num_objects, num_layers;
    struct relay_object objects[4];
    struct relay_layer layers[4];
};
struct relay_decoder { AVCodecContext *codec; };
struct relay_frame { AVFrame *frame; VADRMPRIMESurfaceDescriptor desc; };

static int fail(char *error, int size, const char *where, int code) {
    char detail[AV_ERROR_MAX_STRING_SIZE];
    av_strerror(code, detail, sizeof(detail));
    snprintf(error, size, "%s: %s", where, detail);
    return -1;
}
static enum AVPixelFormat vaapi_format(AVCodecContext *ctx, const enum AVPixelFormat *formats) {
    (void)ctx;
    for (; *formats != AV_PIX_FMT_NONE; ++formats)
        if (*formats == AV_PIX_FMT_VAAPI) return *formats;
    return AV_PIX_FMT_NONE; /* Never silently decode in software. */
}
void relay_decoder_free(struct relay_decoder *decoder) {
    if (!decoder) return;
    avcodec_free_context(&decoder->codec);
    free(decoder);
}
struct relay_decoder *relay_decoder_new(const char *node, char *error, int size) {
    AVBufferRef *device = NULL;
    const AVCodec *codec = avcodec_find_decoder(AV_CODEC_ID_H264);
    if (!codec) { snprintf(error, size, "FFmpeg H.264 decoder unavailable"); return NULL; }
    struct relay_decoder *decoder = calloc(1, sizeof(*decoder));
    if (!decoder) { fail(error, size, "allocate decoder", AVERROR(ENOMEM)); return NULL; }
    int ret = av_hwdevice_ctx_create(&device, AV_HWDEVICE_TYPE_VAAPI, node, NULL, 0);
    if (ret < 0) { fail(error, size, "open VA-API render node", ret); goto failed; }
    decoder->codec = avcodec_alloc_context3(codec);
    if (!decoder->codec) { fail(error, size, "allocate codec", AVERROR(ENOMEM)); goto failed; }
    decoder->codec->hw_device_ctx = device;
    device = NULL;
    decoder->codec->get_format = vaapi_format;
    decoder->codec->thread_count = 1;
    decoder->codec->flags |= AV_CODEC_FLAG_LOW_DELAY;
    ret = avcodec_open2(decoder->codec, codec, NULL);
    if (ret < 0) { fail(error, size, "open H.264 decoder", ret); goto failed; }
    return decoder;
failed:
    av_buffer_unref(&device);
    relay_decoder_free(decoder);
    return NULL;
}
void relay_frame_free(struct relay_frame *frame) {
    if (!frame) return;
    for (uint32_t i = 0; i < frame->desc.num_objects && i < 4; ++i) close(frame->desc.objects[i].fd);
    av_frame_free(&frame->frame);
    free(frame);
}
/* 0 = picture, 1 = needs more input, -1 = failure. */
int relay_decode(struct relay_decoder *decoder, const uint8_t *data, int len,
                 struct relay_frame **out, struct relay_desc *desc, char *error, int size) {
    *out = NULL;
    AVPacket *packet = av_packet_alloc();
    struct relay_frame *frame = calloc(1, sizeof(*frame));
    if (!packet || !frame) {
        av_packet_free(&packet); free(frame);
        return fail(error, size, "allocate packet/frame", AVERROR(ENOMEM));
    }
    int ret = av_new_packet(packet, len); /* Includes FFmpeg's required padding. */
    if (ret < 0) { fail(error, size, "allocate packet data", ret); goto failed; }
    memcpy(packet->data, data, len);
    ret = avcodec_send_packet(decoder->codec, packet);
    av_packet_free(&packet);
    if (ret < 0) { fail(error, size, "send H.264 packet", ret); goto failed; }
    frame->frame = av_frame_alloc();
    if (!frame->frame) { fail(error, size, "allocate frame", AVERROR(ENOMEM)); goto failed; }
    ret = avcodec_receive_frame(decoder->codec, frame->frame);
    if (ret == AVERROR(EAGAIN)) { relay_frame_free(frame); return 1; }
    if (ret < 0) { fail(error, size, "receive H.264 picture", ret); goto failed; }
    AVFrame *picture = frame->frame;
    if (picture->format != AV_PIX_FMT_VAAPI || !picture->hw_frames_ctx) {
        snprintf(error, size, "decoder did not return a VA-API surface"); goto failed;
    }
    AVHWFramesContext *frames = (AVHWFramesContext *)picture->hw_frames_ctx->data;
    if (frames->sw_format != AV_PIX_FMT_NV12) {
        snprintf(error, size, "only 8-bit NV12 AVC420 surfaces are supported"); goto failed;
    }
    AVHWDeviceContext *device = frames->device_ctx;
    AVVAAPIDeviceContext *va = device->hwctx;
    VASurfaceID surface = (VASurfaceID)(uintptr_t)picture->data[3];
    VAStatus status = vaSyncSurface(va->display, surface);
    if (status != VA_STATUS_SUCCESS) {
        snprintf(error, size, "wait for VA-API decode: %s", vaErrorStr(status)); goto failed;
    }
    status = vaExportSurfaceHandle(va->display, surface, VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                                   VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_SEPARATE_LAYERS, &frame->desc);
    if (status != VA_STATUS_SUCCESS) {
        /* Failed exports do not transfer handles. */
        frame->desc.num_objects = 0;
        snprintf(error, size, "export VA-API surface: %s", vaErrorStr(status)); goto failed;
    }
    if (frame->desc.num_objects > 4 || frame->desc.num_layers > 4) {
        snprintf(error, size, "invalid VA-API export descriptor"); goto failed;
    }
    memset(desc, 0, sizeof(*desc));
    desc->width = picture->width;
    desc->height = picture->height;
    desc->allocation_width = frame->desc.width;
    desc->allocation_height = frame->desc.height;
    desc->colorspace = picture->colorspace;
    /* MS-RDPEGFX 3.3.8.3.1 defaults to full-range BT.709. Honor
     * an explicit limited-range VUI from non-Windows/test servers. */
    desc->full_range = picture->color_range != AVCOL_RANGE_MPEG;
    desc->chroma_location = picture->chroma_location;
    desc->num_objects = frame->desc.num_objects;
    desc->num_layers = frame->desc.num_layers;
    for (uint32_t i = 0; i < desc->num_objects; ++i) {
        desc->objects[i].fd = frame->desc.objects[i].fd;
        desc->objects[i].size = frame->desc.objects[i].size;
        desc->objects[i].modifier = frame->desc.objects[i].drm_format_modifier;
    }
    for (uint32_t i = 0; i < desc->num_layers; ++i) {
        desc->layers[i].format = frame->desc.layers[i].drm_format;
        desc->layers[i].num_planes = frame->desc.layers[i].num_planes;
        memcpy(desc->layers[i].object_index, frame->desc.layers[i].object_index, sizeof(desc->layers[i].object_index));
        memcpy(desc->layers[i].offset, frame->desc.layers[i].offset, sizeof(desc->layers[i].offset));
        memcpy(desc->layers[i].pitch, frame->desc.layers[i].pitch, sizeof(desc->layers[i].pitch));
    }
    *out = frame;
    return 0;
failed:
    av_packet_free(&packet);
    relay_frame_free(frame);
    return -1;
}
