// Independent PipeWire consumer for the ignored ScreenCast delivery test.
// cc video_consumer.c -o /tmp/yas-video-consumer $(pkg-config --cflags --libs libpipewire-0.3)
#include <pipewire/pipewire.h>
#include <spa/param/video/format-utils.h>
#include <stdio.h>
struct capture { struct pw_main_loop *loop; struct pw_stream *stream; const char *path; int received; int bpp; };
static void process(void *arg) {
    struct capture *c=arg;
    struct pw_buffer *b=pw_stream_dequeue_buffer(c->stream);
    if(!b) return;
    struct spa_data *d=&b->buffer->datas[0];
    if(d->data && d->chunk && d->chunk->size && d->chunk->offset+d->chunk->size<=d->maxsize) {
        FILE *file=fopen(c->path,"wb");
        if(file) { c->received=fwrite((char*)d->data+d->chunk->offset,1,d->chunk->size,file)==d->chunk->size; fclose(file); }
    }
    pw_stream_queue_buffer(c->stream,b);
    if(c->received) pw_main_loop_quit(c->loop);
}
static void changed(void *arg,uint32_t id,const struct spa_pod *param) {
    struct capture *c=arg;
    if(id!=SPA_PARAM_Format || !param) return;
    uint8_t bytes[1024]; struct spa_pod_builder b=SPA_POD_BUILDER_INIT(bytes,sizeof(bytes));
    const struct spa_pod *buffers=spa_pod_builder_add_object(&b,SPA_TYPE_OBJECT_ParamBuffers,SPA_PARAM_Buffers,SPA_PARAM_BUFFERS_buffers,SPA_POD_CHOICE_RANGE_Int(3,2,8),SPA_PARAM_BUFFERS_blocks,SPA_POD_Int(1),SPA_PARAM_BUFFERS_size,SPA_POD_Int(64*48*c->bpp),SPA_PARAM_BUFFERS_stride,SPA_POD_Int(64*c->bpp));
    pw_stream_update_params(c->stream,&buffers,1);
}
static const struct pw_stream_events events={ PW_VERSION_STREAM_EVENTS, .process=process, .param_changed=changed };
int main(int argc,char **argv) {
    if(argc!=4) return 2;
    pw_init(&argc,&argv);
    struct capture c={.path=argv[3]};
    c.loop=pw_main_loop_new(NULL);
    c.stream=pw_stream_new_simple(pw_main_loop_get_loop(c.loop),"YAS color test",pw_properties_new(PW_KEY_MEDIA_TYPE,"Video",PW_KEY_MEDIA_CATEGORY,"Capture",PW_KEY_MEDIA_ROLE,"Screen",PW_KEY_TARGET_OBJECT,argv[1],NULL),&events,&c);
    int mode=atoi(argv[2]);c.bpp=mode==4 ? 8 : 4;
    struct spa_video_info_raw info={.format=mode==4 ? SPA_VIDEO_FORMAT_RGBA_F16 : mode==3 ? SPA_VIDEO_FORMAT_xBGR_210LE : mode==2 ? SPA_VIDEO_FORMAT_xRGB_210LE : SPA_VIDEO_FORMAT_RGBA,.size={64,48},.framerate={30,1},.color_range=SPA_VIDEO_COLOR_RANGE_0_255,.color_matrix=SPA_VIDEO_COLOR_MATRIX_RGB,.transfer_function=mode>=2 && mode<=4 ? SPA_VIDEO_TRANSFER_SMPTE2084 : SPA_VIDEO_TRANSFER_SRGB,.color_primaries=mode>=2 && mode<=4 ? SPA_VIDEO_COLOR_PRIMARIES_BT2020 : mode==1 ? SPA_VIDEO_COLOR_PRIMARIES_SMPTEEG432 : SPA_VIDEO_COLOR_PRIMARIES_BT709};
    if(mode==5) { info.color_range=SPA_VIDEO_COLOR_RANGE_UNKNOWN; info.color_matrix=SPA_VIDEO_COLOR_MATRIX_UNKNOWN; info.transfer_function=SPA_VIDEO_TRANSFER_UNKNOWN; info.color_primaries=SPA_VIDEO_COLOR_PRIMARIES_UNKNOWN; }
    uint8_t bytes[1024]; struct spa_pod_builder builder=SPA_POD_BUILDER_INIT(bytes,sizeof(bytes));
    const struct spa_pod *param=spa_format_video_raw_build(&builder,SPA_PARAM_EnumFormat,&info);
    int result=pw_stream_connect(c.stream,PW_DIRECTION_INPUT,PW_ID_ANY,PW_STREAM_FLAG_AUTOCONNECT|PW_STREAM_FLAG_MAP_BUFFERS,&param,1);
    if(result>=0) pw_main_loop_run(c.loop);
    pw_stream_destroy(c.stream);pw_main_loop_destroy(c.loop);pw_deinit();
    return c.received ? 0 : 1;
}
