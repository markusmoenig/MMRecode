#include "mmrecode.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void print_last_error(void) {
    char message[512];
    (void)mmr_last_error_message(message, sizeof(message));
    fprintf(stderr, "MMRecode error: %s\n", message);
}

static uint8_t *read_file(const char *path, size_t *length) {
    FILE *file = fopen(path, "rb");
    long file_length;
    uint8_t *data;

    if (file == NULL) {
        return NULL;
    }
    if (fseek(file, 0, SEEK_END) != 0) {
        fclose(file);
        return NULL;
    }
    file_length = ftell(file);
    if (file_length <= 0 || fseek(file, 0, SEEK_SET) != 0) {
        fclose(file);
        return NULL;
    }
    data = (uint8_t *)malloc((size_t)file_length);
    if (data == NULL) {
        fclose(file);
        return NULL;
    }
    if (fread(data, 1, (size_t)file_length, file) != (size_t)file_length) {
        free(data);
        fclose(file);
        return NULL;
    }
    fclose(file);
    *length = (size_t)file_length;
    return data;
}

int main(int argc, char **argv) {
    size_t input_length = 0;
    uint8_t *input;
    mmr_video_frame decoded;
    mmr_video_frame_view view;
    mmr_buffer encoded;
    mmr_status status;
    size_t index;

    if (argc != 2) {
        fprintf(stderr, "usage: %s input.jpg\n", argv[0]);
        return 2;
    }
    if (mmr_abi_version() != MMR_ABI_VERSION || strlen(mmr_version()) == 0) {
        fprintf(stderr, "unexpected MMRecode version\n");
        return 3;
    }

    input = read_file(argv[1], &input_length);
    if (input == NULL) {
        fprintf(stderr, "could not read %s\n", argv[1]);
        return 4;
    }

    memset(&decoded, 0, sizeof(decoded));
    decoded.struct_size = sizeof(decoded);
    status = mmr_mjpeg_decode(input, input_length, &decoded);
    free(input);
    if (status != MMR_STATUS_OK) {
        print_last_error();
        return 5;
    }

    memset(&view, 0, sizeof(view));
    view.struct_size = sizeof(view);
    view.format = decoded.format;
    view.range = decoded.range;
    view.width = decoded.width;
    view.height = decoded.height;
    view.plane_count = decoded.plane_count;
    for (index = 0; index < decoded.plane_count; ++index) {
        view.planes[index].data = decoded.planes[index].data;
        view.planes[index].data_len = decoded.planes[index].data_len;
        view.planes[index].stride = decoded.planes[index].stride;
        view.planes[index].width = decoded.planes[index].width;
        view.planes[index].height = decoded.planes[index].height;
    }

    memset(&encoded, 0, sizeof(encoded));
    encoded.struct_size = sizeof(encoded);
    status = mmr_mjpeg_encode(&view, 75, &encoded);
    if (status != MMR_STATUS_OK) {
        print_last_error();
        mmr_video_frame_free(&decoded);
        return 6;
    }
    if (encoded.len < 4 || encoded.data[0] != 0xff || encoded.data[1] != 0xd8) {
        fprintf(stderr, "encoder did not return a JPEG image\n");
        mmr_buffer_free(&encoded);
        mmr_video_frame_free(&decoded);
        return 7;
    }

    printf(
        "MMRecode C API %u decoded %zux%zu and encoded %zu bytes\n",
        mmr_abi_version(),
        decoded.width,
        decoded.height,
        encoded.len);
    mmr_buffer_free(&encoded);
    mmr_video_frame_free(&decoded);
    return 0;
}

