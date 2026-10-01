#define SEC(name) __attribute__((section(name), used))

SEC("classifier")
int nodemigrate_probe(void *context)
{
    (void)context;
    return 0;
}

char LICENSE[] SEC("license") = "GPL";
