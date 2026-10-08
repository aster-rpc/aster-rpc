#define _GNU_SOURCE
#include "aster_ffi.h"
#include <assert.h>
#include <arpa/inet.h>
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>
static aster_bytes_t bytes(const char *s) { return (aster_bytes_t){(const uint8_t*)s,strlen(s)}; }
static int listener(unsigned short port) {
 int fd=socket(AF_INET,SOCK_STREAM,0), yes=1; assert(fd>=0);
 assert(setsockopt(fd,SOL_SOCKET,SO_REUSEADDR,&yes,sizeof yes)==0);
 struct sockaddr_in a={.sin_family=AF_INET,.sin_port=htons(port),.sin_addr.s_addr=htonl(INADDR_LOOPBACK)};
 assert(bind(fd,(void*)&a,sizeof a)==0);assert(listen(fd,1)==0);return fd;
}
static unsigned short portof(int fd) {struct sockaddr_in a; socklen_t n=sizeof a;assert(getsockname(fd,(void*)&a,&n)==0);return ntohs(a.sin_port);}
static aster_node_t node(aster_runtime_t rt,const char* path) {
 aster_config_t c={0};assert((path ? aster_config_from_toml(bytes(path),&c) : aster_config_from_env(&c))==0);
 c.relay_mode=ASTER_RELAY_MODE_DISABLED;c.bind_addr=bytes("127.0.0.1:0");
 aster_bytes_t alpn=bytes("aster/1");c.alpns=(aster_bytes_list_t){&alpn,1};
 aster_operation_t op=0;assert(aster_node_start(rt,&c,0,0,&op)==0);assert(aster_config_release(&c)==0);
 for(int i=0;i<100;i++){aster_event_t ev={0};if(aster_poll_events(rt,&ev,1,100)&&ev.operation==op){assert(ev.status==0);assert(ev.kind==ASTER_EVENT_NODE_CREATED);if(ev.buffer)assert(aster_buffer_release(rt,ev.buffer)==0);return ev.handle;}}
 assert(!"node creation timeout");return 0;
}
static void toml(const char* path,unsigned short port,int enabled) {FILE*f=fopen(path,"w");assert(f);fprintf(f,"[health]\nport=%u\nhost=\"127.0.0.1\"\nenabled=%s\n",port,enabled?"true":"false");assert(fclose(f)==0);}
static int http(unsigned short port,const char* path) {
 int fd=socket(AF_INET,SOCK_STREAM,0);assert(fd>=0);struct timeval timeout={5,0};setsockopt(fd,SOL_SOCKET,SO_RCVTIMEO,&timeout,sizeof timeout);
 struct sockaddr_in a={.sin_family=AF_INET,.sin_port=htons(port),.sin_addr.s_addr=htonl(INADDR_LOOPBACK)};assert(connect(fd,(void*)&a,sizeof a)==0);
 char buf[4096];int n=snprintf(buf,sizeof buf,"GET %s HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",path);assert(write(fd,buf,n)==n);n=read(fd,buf,sizeof buf-1);assert(n>0);buf[n]=0;int status=0;assert(sscanf(buf,"HTTP/1.1 %d",&status)==1);close(fd);return status;
}
int main(int argc,char**argv) {
 assert(argc==2);Dl_info info;assert(dladdr((void*)aster_health_start,&info));printf("loaded_native=%s\n",info.dli_fname);
 int occupied=listener(0);unsigned short port=portof(occupied);aster_runtime_t rt=0;assert(aster_runtime_new(ASTER_ABI_VERSION_MAJOR,NULL,&rt)==0);
 char envport[8];snprintf(envport,sizeof envport,"%u",port);
 assert(setenv("ASTER_HEALTH_PORT",envport,1)==0);assert(setenv("ASTER_HEALTH_ENABLED","false",1)==0);
 aster_node_t envdisabled=node(rt,NULL);aster_health_t envhealth=0;assert(aster_health_start(rt,envdisabled,NULL,&envhealth)==ASTER_STATUS_INVALID_ARGUMENT);assert(envhealth==0);
 assert(setenv("ASTER_HEALTH_ENABLED","true",1)==0);aster_node_t envenabled=node(rt,NULL);assert(aster_health_start(rt,envenabled,NULL,&envhealth)==ASTER_STATUS_CONNECTION_REFUSED);assert(envhealth==0);
 assert(unsetenv("ASTER_HEALTH_PORT")==0);assert(unsetenv("ASTER_HEALTH_ENABLED")==0);
 toml(argv[1],port,0);aster_node_t disabled=node(rt,argv[1]);aster_health_t h=0;assert(aster_health_start(rt,disabled,NULL,&h)==ASTER_STATUS_INVALID_ARGUMENT);assert(h==0);
 toml(argv[1],port,1);aster_node_t configured=node(rt,argv[1]);int status=aster_health_start(rt,configured,NULL,&h);
 char error[512];size_t errorlen=aster_last_error_message((uint8_t*)error,sizeof error-1);error[errorlen<sizeof error-1?errorlen:sizeof error-1]=0;fprintf(stderr,"configured NULL health status=%d error=%s\n",status,error);
 assert(status==ASTER_STATUS_CONNECTION_REFUSED);assert(h==0);
 aster_health_config_t explicit={.struct_size=sizeof explicit,.port=0,.host=bytes("127.0.0.1")};assert(aster_health_start(rt,configured,&explicit,&h)==0);
 aster_bytes_t addr={0};assert(aster_health_addr(h,&addr)==0);char address[128];assert(addr.len<sizeof address);memcpy(address,addr.ptr,addr.len);address[addr.len]=0;assert(aster_string_release(addr.ptr,addr.len)==0);unsigned short actual=(unsigned short)atoi(strchr(address,':')+1);assert(actual!=port);
 assert(http(actual,"/readyz")==503);assert(aster_health_set_ready(h,1)==0);assert(http(actual,"/readyz")==200);
 assert(aster_health_check_set(h,bytes("config-cache"),0)==0);assert(http(actual,"/readyz")==503);assert(aster_health_check_set(h,bytes("config-cache"),1)==0);assert(http(actual,"/readyz")==200);
 assert(aster_health_stop(rt,h)==0);int rebound=listener(actual);close(rebound);close(occupied);
 assert(aster_health_start(rt,configured,NULL,&h)==0);
 assert(aster_health_set_ready(h,1)==0);assert(http(port,"/readyz")==200);
 assert(aster_health_check_set(h,bytes("config-cache"),0)==0);assert(http(port,"/readyz")==503);
 assert(aster_health_check_set(h,bytes("config-cache"),1)==0);assert(http(port,"/readyz")==200);
 assert(aster_health_stop(rt,h)==0);rebound=listener(port);close(rebound);assert(aster_runtime_close(rt)==0);unlink(argv[1]);puts("PASS disabled/bind/explicit-precedence/readiness/port-reuse");return 0;
}
