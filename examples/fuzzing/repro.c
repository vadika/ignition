#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
void parse(const uint8_t*,unsigned long);
int main(int c,char**v){FILE*f=fopen(v[1],"rb");uint8_t b[65536];size_t n=fread(b,1,sizeof b,f);
  printf("replaying %zu-byte crash input...\n",n);parse(b,n);printf("(no crash)\n");return 0;}
