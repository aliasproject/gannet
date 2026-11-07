// Bake-off probe: GtkListView over a 100,000-row GtkStringList.
#include <gtk/gtk.h>
static void setup(GtkSignalListItemFactory*f,GtkListItem*i,gpointer d){ gtk_list_item_set_child(i,gtk_label_new("")); }
static void bind(GtkSignalListItemFactory*f,GtkListItem*i,gpointer d){ gtk_label_set_text(GTK_LABEL(gtk_list_item_get_child(i)),gtk_string_object_get_string(gtk_list_item_get_item(i))); }
static void act(GtkApplication*a,gpointer d){ GtkStringList*l=gtk_string_list_new(NULL); char b[64]; for(int i=0;i<100000;i++){snprintf(b,64,"file-%d.txt",i);gtk_string_list_append(l,b);}
 GtkListItemFactory*f=gtk_signal_list_item_factory_new(); g_signal_connect(f,"setup",G_CALLBACK(setup),0); g_signal_connect(f,"bind",G_CALLBACK(bind),0);
 GtkWidget*v=gtk_list_view_new(GTK_SELECTION_MODEL(gtk_single_selection_new(G_LIST_MODEL(l))),f); GtkWidget*s=gtk_scrolled_window_new(); gtk_scrolled_window_set_child(GTK_SCROLLED_WINDOW(s),v);
 GtkWidget*w=gtk_application_window_new(a); gtk_window_set_child(GTK_WINDOW(w),s); gtk_window_set_default_size(GTK_WINDOW(w),1200,760); gtk_window_present(GTK_WINDOW(w)); }
int main(int c,char**v){ GtkApplication*a=gtk_application_new("org.gannet.Bake",G_APPLICATION_NON_UNIQUE); g_signal_connect(a,"activate",G_CALLBACK(act),0); return g_application_run(G_APPLICATION(a),c,v); }
