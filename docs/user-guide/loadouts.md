# Download a model collection

A loadout groups selected models from different repositories and folders. It needs one Hoard
server. Membership stores references; model files stay in their original locations.

In the Index, select a model, or select several using Space, and choose ADD TO LOADOUT (L).
Choose a collection and ADD TO THIS LOADOUT, or enter a name and CREATE a new collection.
Multiple checkpoints, VAEs and text encoders can belong to one collection. Adding the same
repository file again does not duplicate it.

Search for the collection name or a member filename in the Index. Use `role:loadout` to list
collections only. A member model's inspector also offers RELATED LOADOUTS. Open a collection
to review its members and share its `#/loadout/<id>` link.

SAVE NAME renames the collection. Each member has a DOWNLOAD PATH relative to the client
folder; SAVE PATH changes that path, and REMOVE removes membership only. DELETE LOADOUT
requires confirmation and leaves every original model file untouched.

Under DOWNLOAD ON YOUR COMPUTER, set the destination to the client folder that should hold
the collection. For ComfyUI this is its `models` folder. Keep these relative paths for the
user-provided Qwen Image 2.1 example:

```text
diffusion_models/qwen_image_2.1_int8_convrot.safetensors
text_encoders/qwen3vl_8b_int8_convrot.safetensors
text_encoders/qwen3.5_9b_qwen_image_2.1_pe_t2i.int8_convrot.safetensors
vae/qwen_image_2.1_vae_bf16.safetensors
```

Choose Windows PowerShell or Linux/macOS shell, review the script, and COPY DOWNLOAD SCRIPT
or SAVE SCRIPT. Run it on the computer receiving the models, where `hf` is installed. Relative
roots such as `./models` are interpreted from that computer's current working directory.
The browser cannot choose or write an arbitrary local directory on its own.

Scripts fetch only the collection's files through the Hoard endpoint. Each transfer stages
under `.hoard-downloads` in the chosen client root, then moves a completed file to its explicit
path. Failed transfers stop the script; rerunning can resume staged downloads. Existing final
files are preserved and reported as skipped, without validating their contents. Review those
warnings before using the collection. No files are installed on the Hoard server by this action.

An unavailable model or a collision between destination paths blocks script generation.
Restore the source, remove the unavailable member, or edit the paths. Multi-file shard artifacts
currently require individual file downloads and cannot be exported through collection scripts.
A collection groups files; it does not establish model compatibility or certify an inference
workflow. The requested example's name does not verify text-to-video capability.
